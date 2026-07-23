//! # aegis-mesh
//!
//! Bounded subagent supervision. A parent Aegis agent can delegate a sub-task to
//! a child agent that runs in an isolated Tokio task under hard token/step/wall-
//! clock bounds. If the child panics, times out, is cancelled, or blows its
//! budget, the failure is contained: it's journaled, surfaced as a structured
//! [`SubagentResult::Error`], and **never propagates into the parent's task or
//! context** — Tokio's task isolation plus an inner `spawn` guarantee the parent
//! keeps running.

use std::sync::Arc;
use std::time::Duration;

use sturdy_core::{
    Action, Budget, Outcome, ReActEngine, Reasoner, Step, StepObserver, Task, TaskId, ToolCall,
    ToolExecutor,
};
use sturdy_ledger::Ledger;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug, Error)]
pub enum MeshError {
    #[error("the subagent is no longer running")]
    ChildGone,
}

/// Hard bounds and identity for a spawned subagent.
#[derive(Debug, Clone)]
pub struct SubagentConfig {
    pub name: String,
    pub max_tokens: u64,
    pub max_steps: u64,
    pub timeout_secs: u64,
    pub parent_run_id: TaskId,
}

impl SubagentConfig {
    /// A conservative default: 20k tokens, 8 steps, 60s.
    pub fn new(name: impl Into<String>, parent_run_id: TaskId) -> Self {
        SubagentConfig {
            name: name.into(),
            max_tokens: 20_000,
            max_steps: 8,
            timeout_secs: 60,
            parent_run_id,
        }
    }
}

/// Parent → child control messages.
#[derive(Debug, Clone)]
pub enum SubagentCommand {
    /// Cooperative pause request (best-effort; not enforced mid-step in this MVP).
    Pause,
    /// Cancel the child immediately.
    Cancel,
    /// Ask the child to emit its current status on the event stream.
    RequestStatus,
}

/// Child → parent streaming events. The parent renders these live.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentEvent {
    StepCompleted { index: u32 },
    ToolInvoked { name: String },
    Finished { answer: String, tokens: u64 },
    Failed { reason: String, tokens: u64 },
}

/// The terminal outcome handed back to the parent.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentResult {
    Ok { summary: String, tokens_used: u64 },
    Error { reason: String, tokens_used: u64 },
}

impl SubagentResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, SubagentResult::Ok { .. })
    }
}

/// A live handle to a supervised subagent.
pub struct SubagentHandle {
    events: mpsc::Receiver<SubagentEvent>,
    commands: mpsc::Sender<SubagentCommand>,
    join: JoinHandle<SubagentResult>,
}

impl SubagentHandle {
    /// Send a control command to the child.
    pub async fn command(&self, cmd: SubagentCommand) -> Result<(), MeshError> {
        self.commands
            .send(cmd)
            .await
            .map_err(|_| MeshError::ChildGone)
    }

    /// Fire-and-forget cancel (safe even if the child already exited).
    pub fn cancel(&self) {
        let _ = self.commands.try_send(SubagentCommand::Cancel);
    }

    /// Await the next streamed event, or `None` once the child is done.
    pub async fn next_event(&mut self) -> Option<SubagentEvent> {
        self.events.recv().await
    }

    /// Block until the child terminates and collect its result. Always resolves
    /// to a value — a panicked child yields a structured error, never a panic.
    pub async fn wait(self) -> SubagentResult {
        match self.join.await {
            Ok(result) => result,
            Err(_) => SubagentResult::Error {
                reason: "subagent supervisor task failed".into(),
                tokens_used: 0,
            },
        }
    }
}

/// Spawn `reasoner`/`tools` as a bounded subagent working on `task_prompt`.
pub fn spawn_subagent(
    config: SubagentConfig,
    task_prompt: impl Into<String>,
    reasoner: Arc<dyn Reasoner>,
    tools: Arc<dyn ToolExecutor>,
    ledger: Option<Arc<Ledger>>,
) -> SubagentHandle {
    let (ev_tx, ev_rx) = mpsc::channel(128);
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let prompt = task_prompt.into();
    let join = tokio::spawn(supervise(
        config, prompt, reasoner, tools, ledger, ev_tx, cmd_rx,
    ));
    SubagentHandle {
        events: ev_rx,
        commands: cmd_tx,
        join,
    }
}

/// Streams engine steps to the parent as [`SubagentEvent`]s.
struct ChannelObserver {
    tx: mpsc::Sender<SubagentEvent>,
}

impl StepObserver for ChannelObserver {
    fn on_step(&self, _task: &Task, step: &Step) {
        // try_send: streaming is best-effort; a full/closed channel never blocks
        // or fails the agent loop.
        let _ = self
            .tx
            .try_send(SubagentEvent::StepCompleted { index: step.index });
        if let Action::Tool(call) = &step.action {
            let _ = self.tx.try_send(SubagentEvent::ToolInvoked {
                name: call.name.clone(),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    config: SubagentConfig,
    prompt: String,
    reasoner: Arc<dyn Reasoner>,
    tools: Arc<dyn ToolExecutor>,
    ledger: Option<Arc<Ledger>>,
    ev_tx: mpsc::Sender<SubagentEvent>,
    mut cmd_rx: mpsc::Receiver<SubagentCommand>,
) -> SubagentResult {
    let budget = Budget {
        max_tokens: config.max_tokens,
        max_steps: config.max_steps,
        wall_clock: Duration::from_secs(config.timeout_secs),
    };
    let tracker = budget.tracker();
    // A clone lets us read token usage even after aborting the inner task.
    let tracker_read = tracker.clone();
    let engine = ReActEngine::new(reasoner, tools, tracker)
        .with_observer(Arc::new(ChannelObserver { tx: ev_tx.clone() }));
    let task = Task::new(prompt);

    // Run the agent in its OWN task: a panic there surfaces as a JoinError here
    // instead of unwinding the supervisor (and, in turn, the parent).
    let mut inner: JoinHandle<(Outcome, _)> = tokio::spawn(async move { engine.run(&task).await });

    let outer_timeout = tokio::time::sleep(Duration::from_secs(config.timeout_secs));
    tokio::pin!(outer_timeout);

    let result = loop {
        tokio::select! {
            joined = &mut inner => break match joined {
                Ok((outcome, _traj)) => outcome_to_result(outcome, tracker_read.tokens_used()),
                Err(e) => {
                    let reason = if e.is_panic() { "subagent panicked" } else { "subagent aborted" };
                    SubagentResult::Error { reason: reason.into(), tokens_used: tracker_read.tokens_used() }
                }
            },
            _ = &mut outer_timeout => {
                inner.abort();
                break SubagentResult::Error {
                    reason: format!("timed out after {}s (hard wall-clock bound)", config.timeout_secs),
                    tokens_used: tracker_read.tokens_used(),
                };
            }
            cmd = cmd_rx.recv() => match cmd {
                Some(SubagentCommand::Cancel) => {
                    inner.abort();
                    break SubagentResult::Error {
                        reason: "cancelled by parent".into(),
                        tokens_used: tracker_read.tokens_used(),
                    };
                }
                // Pause/RequestStatus are acknowledged; enforcement is a follow-on.
                Some(SubagentCommand::RequestStatus) => {
                    let _ = ev_tx.try_send(SubagentEvent::StepCompleted { index: tracker_read.steps_used() as u32 });
                    continue;
                }
                Some(SubagentCommand::Pause) => continue,
                None => continue, // parent dropped the command channel; keep running
            }
        }
    };

    // Terminal bookkeeping: stream a final event and journal any failure. Failures
    // are contained here — the parent only ever sees the returned `SubagentResult`.
    match &result {
        SubagentResult::Ok {
            summary,
            tokens_used,
        } => {
            let _ = ev_tx.try_send(SubagentEvent::Finished {
                answer: summary.clone(),
                tokens: *tokens_used,
            });
        }
        SubagentResult::Error {
            reason,
            tokens_used,
        } => {
            let _ = ev_tx.try_send(SubagentEvent::Failed {
                reason: reason.clone(),
                tokens: *tokens_used,
            });
            if let Some(ledger) = &ledger {
                let _ = ledger.record_event(
                    config.parent_run_id,
                    &sturdy_ledger::Event::SubagentFailed {
                        name: config.name.clone(),
                        reason: reason.clone(),
                        tokens_used: *tokens_used,
                    },
                );
            }
        }
    }
    result
}

fn outcome_to_result(outcome: Outcome, tokens: u64) -> SubagentResult {
    match outcome {
        Outcome::Finished { answer } => SubagentResult::Ok {
            summary: answer,
            tokens_used: tokens,
        },
        Outcome::BudgetExhausted { reason } => SubagentResult::Error {
            reason: format!("budget exhausted: {reason}"),
            tokens_used: tokens,
        },
        Outcome::Failed { reason } | Outcome::Interrupted { reason } => SubagentResult::Error {
            reason,
            tokens_used: tokens,
        },
    }
}

// ── parent-side delegation tool ──

/// What a [`SubagentFactory`] produces for a delegated task: the child's bounds
/// plus the reasoner and tools to run it with.
pub type SubagentBuild = (SubagentConfig, Arc<dyn Reasoner>, Arc<dyn ToolExecutor>);

/// Builds the reasoner/tools/config for a named subagent type. Lets a parent's
/// `aegis_delegate_task` tool spin up the right kind of child on demand.
pub trait SubagentFactory: Send + Sync {
    fn build(&self, subagent_type: &str, parent_run_id: TaskId) -> Option<SubagentBuild>;
}

/// A [`ToolExecutor`] a parent agent can be given so its ReAct loop can call
/// `aegis_delegate_task({"subagent_type": "...", "prompt": "..."})` and receive a
/// summarized result — with all subagent failures contained.
pub struct DelegateTool {
    factory: Arc<dyn SubagentFactory>,
    parent_run_id: TaskId,
    ledger: Option<Arc<Ledger>>,
}

impl DelegateTool {
    pub const TOOL_NAME: &'static str = "aegis_delegate_task";

    pub fn new(
        factory: Arc<dyn SubagentFactory>,
        parent_run_id: TaskId,
        ledger: Option<Arc<Ledger>>,
    ) -> Self {
        DelegateTool {
            factory,
            parent_run_id,
            ledger,
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for DelegateTool {
    async fn execute(&self, call: &ToolCall) -> sturdy_core::Result<sturdy_core::Observation> {
        let subagent_type = call
            .arguments
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        let prompt = call
            .arguments
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let Some((config, reasoner, tools)) = self.factory.build(subagent_type, self.parent_run_id)
        else {
            return Ok(sturdy_core::Observation::error(format!(
                "unknown subagent type `{subagent_type}`"
            )));
        };

        let handle = spawn_subagent(config, prompt, reasoner, tools, self.ledger.clone());
        // Delegation is contained: a child failure becomes a tool *observation*
        // the parent can react to, not a fault in the parent's run.
        Ok(match handle.wait().await {
            SubagentResult::Ok { summary, .. } => sturdy_core::Observation::ok(summary),
            SubagentResult::Error { reason, .. } => sturdy_core::Observation::error(reason),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sturdy_core::{Decision, Observation, Reasoner, Thought, Trajectory};

    /// A reasoner that never finishes — always issues another tool call. Drives the
    /// engine straight into its step budget (a classic runaway loop).
    struct LoopingReasoner;
    #[async_trait]
    impl Reasoner for LoopingReasoner {
        async fn next_action(&self, _t: &Task, _tr: &Trajectory) -> sturdy_core::Result<Decision> {
            Ok(Decision {
                thought: Thought("loop forever".into()),
                action: Action::Tool(ToolCall::new("noop", serde_json::json!({}))),
                tokens: 1,
            })
        }
    }

    /// A reasoner that hangs on its first think — the step budget never triggers,
    /// so only the hard wall-clock timeout can rescue the parent.
    struct HangingReasoner;
    #[async_trait]
    impl Reasoner for HangingReasoner {
        async fn next_action(&self, _t: &Task, _tr: &Trajectory) -> sturdy_core::Result<Decision> {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    struct NoopTool;
    #[async_trait]
    impl ToolExecutor for NoopTool {
        async fn execute(&self, _c: &ToolCall) -> sturdy_core::Result<Observation> {
            Ok(Observation::ok("ok"))
        }
    }

    #[tokio::test]
    async fn runaway_subagent_is_capped_at_max_steps_and_parent_survives() {
        let ledger = Arc::new(Ledger::in_memory().unwrap());
        let parent = TaskId::new();
        let mut cfg = SubagentConfig::new("runaway", parent);
        cfg.max_steps = 5;
        cfg.timeout_secs = 60; // won't trigger; the step budget bites first

        let handle = spawn_subagent(
            cfg,
            "loop please",
            Arc::new(LoopingReasoner),
            Arc::new(NoopTool),
            Some(ledger.clone()),
        );
        let result = handle.wait().await;

        // Terminated by the step budget, not by hanging the runtime.
        match &result {
            SubagentResult::Error { reason, .. } => assert!(reason.contains("budget")),
            other => panic!("expected budget error, got {other:?}"),
        }
        // The failure was journaled…
        let events = ledger.events(parent).unwrap();
        assert!(events.iter().any(
            |e| matches!(e, sturdy_ledger::Event::SubagentFailed { name, .. } if name == "runaway")
        ));

        // …and the PARENT runtime is completely unharmed.
        let alive = tokio::spawn(async { 2 + 2 }).await.unwrap();
        assert_eq!(alive, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_subagent_is_killed_by_timeout() {
        let parent = TaskId::new();
        let mut cfg = SubagentConfig::new("hanger", parent);
        cfg.timeout_secs = 2;
        cfg.max_steps = 1_000_000; // never reached

        let handle = spawn_subagent(
            cfg,
            "hang",
            Arc::new(HangingReasoner),
            Arc::new(NoopTool),
            None,
        );
        // With paused time, the runtime auto-advances to the timeout instantly.
        let result = handle.wait().await;
        match result {
            SubagentResult::Error { reason, .. } => assert!(reason.contains("timed out")),
            other => panic!("expected timeout error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_tool_contains_child_failure_as_observation() {
        struct Factory;
        impl SubagentFactory for Factory {
            fn build(&self, subagent_type: &str, parent: TaskId) -> Option<SubagentBuild> {
                if subagent_type != "looper" {
                    return None;
                }
                let mut cfg = SubagentConfig::new("looper", parent);
                cfg.max_steps = 3;
                Some((cfg, Arc::new(LoopingReasoner), Arc::new(NoopTool)))
            }
        }

        let tool = DelegateTool::new(Arc::new(Factory), TaskId::new(), None);
        let call = ToolCall::new(
            DelegateTool::TOOL_NAME,
            serde_json::json!({ "subagent_type": "looper", "prompt": "go" }),
        );
        let obs = tool.execute(&call).await.unwrap();
        // A runaway child yields an *error observation* the parent can react to —
        // it does not fault the parent's run.
        assert!(format!("{obs:?}").to_lowercase().contains("budget"));
    }
}
