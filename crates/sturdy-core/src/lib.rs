//! # sturdy-core
//!
//! The dependency root of SturdyHarness: domain models, the ReAct execution
//! engine and its state machine, hard budget enforcement, and the shared error
//! taxonomy. Everything here is pure (no I/O, no network) and heavily tested,
//! so the satellite crates can build on a stable, verifiable core.

pub mod budget;
pub mod error;
pub mod model;
pub mod react;

pub use budget::{Budget, BudgetTracker};
pub use error::{BudgetKind, HarnessError, Result};
pub use model::{Action, Observation, Outcome, Step, Task, TaskId, Thought, ToolCall, Trajectory};
pub use react::{Decision, Phase, ReActEngine, Reasoner, StateMachine, StepObserver, ToolExecutor};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // ── budget ──

    #[test]
    fn token_budget_trips_on_overshoot() {
        let t = Budget {
            max_tokens: 100,
            max_steps: 10,
            wall_clock: Duration::from_secs(60),
        }
        .tracker();
        assert!(t.charge_tokens(60).is_ok());
        let err = t.charge_tokens(60).unwrap_err();
        assert!(err.is_budget());
        assert_eq!(t.tokens_used(), 120);
    }

    #[test]
    fn step_budget_trips_after_limit() {
        let t = Budget {
            max_tokens: 1_000,
            max_steps: 2,
            wall_clock: Duration::from_secs(60),
        }
        .tracker();
        assert!(t.charge_step().is_ok());
        assert!(t.charge_step().is_ok());
        assert!(t.charge_step().unwrap_err().is_budget());
    }

    // ── state machine ──

    #[test]
    fn state_machine_accepts_the_react_cycle() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.current(), Phase::Think);
        sm.advance(Phase::Act).unwrap();
        sm.advance(Phase::Observe).unwrap();
        sm.advance(Phase::Think).unwrap();
        sm.advance(Phase::Act).unwrap();
        sm.advance(Phase::Done).unwrap();
    }

    #[test]
    fn state_machine_rejects_illegal_transition() {
        let mut sm = StateMachine::new();
        // Think -> Observe is not a valid edge.
        let err = sm.advance(Phase::Observe).unwrap_err();
        assert!(matches!(err, HarnessError::InvalidTransition { .. }));
    }

    // ── engine ──

    /// A scripted reasoner: emits N tool calls, then finishes.
    struct ScriptedReasoner {
        tool_calls: u32,
        seen: AtomicU32,
    }

    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn next_action(&self, _task: &Task, _traj: &Trajectory) -> Result<Decision> {
            let n = self.seen.fetch_add(1, Ordering::SeqCst);
            let action = if n < self.tool_calls {
                Action::Tool(ToolCall::new("echo", serde_json::json!({ "n": n })))
            } else {
                Action::Finish {
                    answer: "done".into(),
                }
            };
            Ok(Decision {
                thought: Thought(format!("step {n}")),
                action,
                tokens: 10,
            })
        }
    }

    /// A reasoner that never finishes — used to drive budget exhaustion.
    struct NeverFinishes;

    #[async_trait]
    impl Reasoner for NeverFinishes {
        async fn next_action(&self, _task: &Task, _traj: &Trajectory) -> Result<Decision> {
            Ok(Decision {
                thought: Thought("again".into()),
                action: Action::Tool(ToolCall::new("noop", serde_json::Value::Null)),
                tokens: 10,
            })
        }
    }

    struct EchoTool;

    #[async_trait]
    impl ToolExecutor for EchoTool {
        async fn execute(&self, call: &ToolCall) -> Result<Observation> {
            Ok(Observation::ok(format!("ran {}", call.name)))
        }
    }

    #[tokio::test]
    async fn engine_finishes_and_records_every_step() {
        let engine = ReActEngine::new(
            Arc::new(ScriptedReasoner {
                tool_calls: 3,
                seen: AtomicU32::new(0),
            }),
            Arc::new(EchoTool),
            Budget::standard().tracker(),
        );
        let task = Task::new("test goal");
        let (outcome, traj) = engine.run(&task).await;

        match outcome {
            Outcome::Finished { answer } => assert_eq!(answer, "done"),
            other => panic!("expected Finished, got {other:?}"),
        }
        // 3 tool steps + 1 finish step.
        assert_eq!(traj.len(), 4);
        assert_eq!(traj.total_tokens(), 40);
        // The finish step has no observation; tool steps do.
        assert!(traj.steps.last().unwrap().observation.is_none());
        assert!(traj.steps[0].observation.is_some());
    }

    #[tokio::test]
    async fn engine_stops_cleanly_on_step_budget() {
        let budget = Budget {
            max_tokens: 1_000_000,
            max_steps: 4,
            wall_clock: Duration::from_secs(60),
        };
        let engine = ReActEngine::new(
            Arc::new(NeverFinishes),
            Arc::new(EchoTool),
            budget.tracker(),
        );
        let (outcome, traj) = engine.run(&Task::new("loop forever")).await;

        assert!(matches!(outcome, Outcome::BudgetExhausted { .. }));
        // 4 steps recorded, then the 5th charge trips the ceiling.
        assert_eq!(traj.len(), 4);
    }
}
