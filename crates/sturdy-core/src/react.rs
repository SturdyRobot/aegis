//! The ReAct execution engine and its state machine.
//!
//! The engine drives a strict `Think → Act → Observe` cycle until the agent
//! emits a `Finish`, a budget is exhausted, or a fault occurs. Every transition
//! is validated by an explicit [`StateMachine`], and every step is charged
//! against a shared [`BudgetTracker`] before any work is done — so the harness
//! is deterministic and can never run away.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tracing::Instrument;

use crate::budget::BudgetTracker;
use crate::error::{HarnessError, Result};
use crate::model::{Action, Observation, Outcome, Step, Task, Thought, ToolCall, Trajectory};

/// The phase of a single ReAct step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// About to ask the reasoner for a thought + action.
    Think,
    /// Holding an action, about to dispatch it.
    Act,
    /// Holding a tool result, about to fold it into the trajectory.
    Observe,
    /// Terminal.
    Done,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Think => "Think",
            Phase::Act => "Act",
            Phase::Observe => "Observe",
            Phase::Done => "Done",
        }
    }

    /// The legal transitions of the ReAct cycle.
    fn can_transition_to(self, next: Phase) -> bool {
        matches!(
            (self, next),
            (Phase::Think, Phase::Act)
                | (Phase::Act, Phase::Observe) // tool call
                | (Phase::Act, Phase::Done)    // finish
                | (Phase::Observe, Phase::Think) // loop
        )
    }
}

/// A minimal, self-validating state machine over [`Phase`].
#[derive(Debug)]
pub struct StateMachine {
    current: Phase,
}

impl StateMachine {
    pub fn new() -> Self {
        StateMachine {
            current: Phase::Think,
        }
    }

    pub fn current(&self) -> Phase {
        self.current
    }

    /// Advance to `next`, rejecting any transition outside the ReAct cycle.
    pub fn advance(&mut self, next: Phase) -> Result<()> {
        if !self.current.can_transition_to(next) {
            return Err(HarnessError::InvalidTransition {
                from: self.current.name(),
                to: next.name(),
            });
        }
        self.current = next;
        Ok(())
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

/// One decision from the reasoner: a thought, the action to take, and the token
/// cost the model reported for producing it.
#[derive(Debug, Clone)]
pub struct Decision {
    pub thought: Thought,
    pub action: Action,
    pub tokens: u64,
}

/// The policy that decides the next action — typically an LLM client. It sees
/// the task and the full trajectory so far.
#[async_trait]
pub trait Reasoner: Send + Sync {
    async fn next_action(&self, task: &Task, trajectory: &Trajectory) -> Result<Decision>;
}

/// Executes a tool call and returns an observation. Fatal failures return
/// `Err`; *recoverable* tool errors return `Ok(Observation::error(..))` so the
/// agent can react to them.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> Result<Observation>;
}

/// Notified as each step is finalized — the ledger uses this to journal live.
pub trait StepObserver: Send + Sync {
    fn on_step(&self, task: &Task, step: &Step);
}

/// The engine that runs a [`Task`] to an [`Outcome`].
pub struct ReActEngine {
    reasoner: Arc<dyn Reasoner>,
    tools: Arc<dyn ToolExecutor>,
    budget: BudgetTracker,
    observer: Option<Arc<dyn StepObserver>>,
}

impl ReActEngine {
    pub fn new(
        reasoner: Arc<dyn Reasoner>,
        tools: Arc<dyn ToolExecutor>,
        budget: BudgetTracker,
    ) -> Self {
        ReActEngine {
            reasoner,
            tools,
            budget,
            observer: None,
        }
    }

    /// Attach a step observer (e.g. the SQLite ledger).
    pub fn with_observer(mut self, observer: Arc<dyn StepObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn budget(&self) -> &BudgetTracker {
        &self.budget
    }

    /// Drive the task to completion. Never panics; always returns a trajectory
    /// alongside the outcome, even on budget exhaustion or fault.
    pub async fn run(&self, task: &Task) -> (Outcome, Trajectory) {
        let mut trajectory = Trajectory::new(task.id);
        match self.run_inner(task, &mut trajectory).await {
            Ok(outcome) => (outcome, trajectory),
            Err(err) if err.is_budget() => (
                Outcome::BudgetExhausted {
                    reason: err.to_string(),
                },
                trajectory,
            ),
            Err(err) => (
                Outcome::Failed {
                    reason: err.to_string(),
                },
                trajectory,
            ),
        }
    }

    /// Continue a previously-journaled run from where it stopped. `prior` is the
    /// trajectory replayed from the ledger; its steps are **not re-executed** —
    /// they are seeded as context and the engine drives forward from the next
    /// index, so already-performed (possibly side-effecting) tool calls are never
    /// repeated. The continuation runs under a fresh budget.
    pub async fn resume(&self, task: &Task, prior: Trajectory) -> (Outcome, Trajectory) {
        let mut trajectory = prior;
        match self.run_inner(task, &mut trajectory).await {
            Ok(outcome) => (outcome, trajectory),
            Err(err) if err.is_budget() => (
                Outcome::BudgetExhausted {
                    reason: err.to_string(),
                },
                trajectory,
            ),
            Err(err) => (
                Outcome::Failed {
                    reason: err.to_string(),
                },
                trajectory,
            ),
        }
    }

    #[tracing::instrument(
        name = "react.run",
        skip_all,
        fields(agent_id = %task.id, max_steps = self.budget.budget().max_steps)
    )]
    async fn run_inner(&self, task: &Task, trajectory: &mut Trajectory) -> Result<Outcome> {
        // Seed from the trajectory length so a resumed run keeps counting up
        // instead of colliding with already-journaled step indices.
        let mut index: u32 = trajectory.steps.len() as u32;
        loop {
            let mut sm = StateMachine::new(); // fresh cycle per step
            let started = Instant::now();

            // One span per ReAct step; the Think/Act awaits below nest under it,
            // so a trace reads as run → step → {llm, tool}.
            let step_span = tracing::info_span!(
                "react.step",
                step_id = index,
                tokens_used = self.budget.tokens_used(),
                steps_used = self.budget.steps_used(),
            );

            // Budget gates fire *before* any expensive work.
            self.budget.check_deadline()?;
            self.budget.charge_step()?;

            // ── Think ──
            let decision = self
                .budget
                .run_within(self.reasoner.next_action(task, trajectory))
                .instrument(step_span.clone())
                .await?;
            self.budget.charge_tokens(decision.tokens)?;
            sm.advance(Phase::Act)?;

            // ── Act / Observe ──
            let (observation, done_answer) = match &decision.action {
                Action::Finish { answer } => {
                    sm.advance(Phase::Done)?;
                    (None, Some(answer.clone()))
                }
                Action::Tool(call) => {
                    let obs = self
                        .budget
                        .run_within(self.tools.execute(call))
                        .instrument(step_span.clone())
                        .await?;
                    sm.advance(Phase::Observe)?;
                    (Some(obs), None)
                }
            };

            let step = Step {
                index,
                thought: decision.thought,
                action: decision.action,
                observation,
                tokens: decision.tokens,
                elapsed_ms: started.elapsed().as_millis() as u64,
            };
            if let Some(obs) = self.observer.as_ref() {
                obs.on_step(task, &step);
            }
            trajectory.push(step);

            if let Some(answer) = done_answer {
                return Ok(Outcome::Finished { answer });
            }

            index += 1;
            // Loop back: Observe -> Think.
            sm.advance(Phase::Think)?;
        }
    }
}
