//! # kedge-web
//!
//! WebAssembly bindings that run the **real** Kedge ReAct engine
//! ([`kedge_core::ReActEngine`]) directly in the browser — the same deterministic
//! state machine and hard budget enforcement as the CLI, compiled to wasm.
//!
//! This is the landing-page hero demo: a visitor types a goal, clicks Run, and
//! watches the Think → Act → Observe cycle execute live, entirely client-side —
//! no server, no API key, no network. The reasoner and tools are scripted stubs
//! (a browser sandbox has nothing real to call), but the *engine* driving them,
//! the state-machine validation, the trajectory, and the budget ceilings are the
//! genuine article.
//!
//! JS usage:
//! ```js
//! import init, { WasmKedgeAgent } from "./kedge_web.js";
//! await init();
//! const agent = new WasmKedgeAgent('{"max_steps":6}');
//! const answer = await agent.execute("Refactor the auth module", (stepJson) => {
//!   render(JSON.parse(stepJson)); // {index, thought, action, observation, ...}
//! });
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use js_sys::Function;
use wasm_bindgen::prelude::*;

use kedge_core::{
    Action, Budget, Decision, Observation, Outcome, ReActEngine, Reasoner, Result as CoreResult,
    Step, StepObserver, Task, Thought, ToolCall, ToolExecutor, Trajectory,
};

/// Install the panic hook once so a Rust panic surfaces as `console.error`
/// instead of an opaque `unreachable` trap. Safe to call more than once.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// A browser-hosted Kedge agent. Thin wrapper that wires the real engine to a
/// scripted reasoner + tool and streams each finalized step out to JS.
#[wasm_bindgen]
pub struct WasmKedgeAgent {
    max_steps: u64,
    max_tokens: u64,
}

#[derive(serde::Deserialize, Default)]
struct AgentConfig {
    max_steps: Option<u64>,
    max_tokens: Option<u64>,
}

#[wasm_bindgen]
impl WasmKedgeAgent {
    /// Construct from an optional JSON config string, e.g. `'{"max_steps":6}'`.
    /// An empty string uses the demo defaults (6 steps, 10k tokens).
    #[wasm_bindgen(constructor)]
    pub fn new(config_json: &str) -> Result<WasmKedgeAgent, JsValue> {
        let cfg: AgentConfig = if config_json.trim().is_empty() {
            AgentConfig::default()
        } else {
            serde_json::from_str(config_json).map_err(|e| JsValue::from_str(&e.to_string()))?
        };
        Ok(WasmKedgeAgent {
            max_steps: cfg.max_steps.unwrap_or(6),
            max_tokens: cfg.max_tokens.unwrap_or(10_000),
        })
    }

    /// Run the agent on `prompt`. `callback` is invoked once per finalized step
    /// with that step serialized as a JSON string; the returned Promise resolves
    /// to the agent's final answer (or a `(stopped: …)` reason).
    pub async fn execute(&self, prompt: String, callback: Function) -> Result<String, JsValue> {
        // The observer records each step as JSON while the engine runs. We can't
        // hold the JS `Function` inside the observer (it isn't Send+Sync, which
        // the trait requires), so we collect here and replay to `callback` after
        // the run — the browser UI supplies the live pacing.
        let steps: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let observer = Arc::new(StreamObserver {
            steps: steps.clone(),
        });

        let budget = Budget {
            max_tokens: self.max_tokens,
            max_steps: self.max_steps,
            wall_clock: Duration::from_secs(30),
        };
        let engine = ReActEngine::new(
            Arc::new(ScriptedReasoner),
            Arc::new(BrowserTool),
            budget.tracker(),
        )
        .with_observer(observer);

        let task = Task::new(prompt);
        let (outcome, _trajectory) = engine.run(&task).await;

        for step_json in steps.lock().unwrap().iter() {
            // Ignore a throwing callback — one bad render shouldn't abort the run.
            let _ = callback.call1(&JsValue::NULL, &JsValue::from_str(step_json));
        }

        Ok(match outcome {
            Outcome::Finished { answer } => answer,
            Outcome::BudgetExhausted { reason }
            | Outcome::Failed { reason }
            | Outcome::Interrupted { reason } => format!("(stopped: {reason})"),
        })
    }
}

/// Collects each finalized step as a JSON string for later replay to JS.
struct StreamObserver {
    steps: Arc<Mutex<Vec<String>>>,
}

impl StepObserver for StreamObserver {
    fn on_step(&self, _task: &Task, step: &Step) {
        if let Ok(json) = serde_json::to_string(step) {
            self.steps.lock().unwrap().push(json);
        }
    }
}

/// A deterministic, no-network reasoner. It scripts a short, believable
/// Think → Act → Observe → Finish arc off the trajectory length, so the demo is
/// reproducible and never depends on an LLM or an API key.
struct ScriptedReasoner;

#[async_trait]
impl Reasoner for ScriptedReasoner {
    async fn next_action(&self, task: &Task, trajectory: &Trajectory) -> CoreResult<Decision> {
        let (thought, action, tokens): (&str, Action, u64) = match trajectory.len() {
            0 => (
                "First, understand the goal and gather the relevant context.",
                Action::Tool(ToolCall::new(
                    "read_context",
                    serde_json::json!({ "goal": task.goal }),
                )),
                24,
            ),
            1 => (
                "I have the context — inspect the specific code paths involved.",
                Action::Tool(ToolCall::new(
                    "search_code",
                    serde_json::json!({ "query": task.goal }),
                )),
                19,
            ),
            2 => (
                "Enough gathered — apply the change and verify it.",
                Action::Tool(ToolCall::new(
                    "run_verification",
                    serde_json::json!({ "suite": "all" }),
                )),
                21,
            ),
            _ => (
                "Verification passed. I can answer now.",
                Action::Finish {
                    answer: format!(
                        "Completed \"{}\" in {} steps — every step ran inside Kedge's \
                         deterministic ReAct engine, state-machine-validated and \
                         budget-bounded, executing natively in your browser via WebAssembly.",
                        task.goal,
                        trajectory.len() + 1
                    ),
                },
                14,
            ),
        };
        Ok(Decision {
            thought: Thought(thought.to_string()),
            action,
            tokens,
        })
    }
}

/// A stub tool executor: there is nothing real to call in a browser sandbox, so
/// it returns believable mock observations. The engine, state machine, budgets,
/// and trajectory around it are all real.
struct BrowserTool;

#[async_trait]
impl ToolExecutor for BrowserTool {
    async fn execute(&self, call: &ToolCall) -> CoreResult<Observation> {
        let content = match call.name.as_str() {
            "read_context" => "Loaded 3 relevant files and the module's public API surface.",
            "search_code" => "Found 2 call sites and 1 test covering the target behavior.",
            "run_verification" => "All checks passed: build ✓, 41 tests ✓, lints ✓.",
            other => return Ok(Observation::ok(format!("[{other}] returned mock data."))),
        };
        Ok(Observation::ok(content))
    }
}
