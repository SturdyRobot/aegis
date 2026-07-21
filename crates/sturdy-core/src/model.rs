//! Domain model for a ReAct trajectory.
//!
//! A run is a [`Task`] that produces a [`Trajectory`]: an ordered list of
//! [`Step`]s, each a (thought → action → observation) triple. Everything here
//! is `serde`-serializable so the ledger can persist and replay it byte-for-byte.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for a task/run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    pub fn new() -> Self {
        TaskId(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The unit of work handed to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    /// Natural-language objective the agent must satisfy.
    pub goal: String,
    /// Optional working directory the tools operate against.
    #[serde(default)]
    pub workspace: Option<String>,
}

impl Task {
    pub fn new(goal: impl Into<String>) -> Self {
        Task {
            id: TaskId::new(),
            goal: goal.into(),
            workspace: None,
        }
    }

    pub fn in_workspace(mut self, dir: impl Into<String>) -> Self {
        self.workspace = Some(dir.into());
        self
    }
}

/// A model's private reasoning for a step (the "Re" in ReAct).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Thought(pub String);

/// A request to invoke a named tool with JSON arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        ToolCall {
            name: name.into(),
            arguments,
        }
    }
}

/// What the agent decided to do this step (the "Act").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Call a tool and observe the result.
    Tool(ToolCall),
    /// Terminate successfully with a final answer.
    Finish { answer: String },
}

/// The result of an action (the "Observation").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub content: String,
    /// True when the tool reported failure — the agent can still recover.
    #[serde(default)]
    pub is_error: bool,
}

impl Observation {
    pub fn ok(content: impl Into<String>) -> Self {
        Observation {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Observation {
            content: content.into(),
            is_error: true,
        }
    }
}

/// One fully-resolved ReAct step, ready to be journaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Zero-based position in the trajectory.
    pub index: u32,
    pub thought: Thought,
    pub action: Action,
    /// `None` for a `Finish` action (there is nothing to observe).
    pub observation: Option<Observation>,
    /// Tokens the reasoner reported consuming for this step.
    pub tokens: u64,
    /// Wall-clock spent producing + executing this step.
    pub elapsed_ms: u64,
}

/// The complete record of a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    pub task_id: TaskId,
    pub steps: Vec<Step>,
}

impl Trajectory {
    pub fn new(task_id: TaskId) -> Self {
        Trajectory {
            task_id,
            steps: Vec::new(),
        }
    }

    pub fn push(&mut self, step: Step) {
        self.steps.push(step);
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Total tokens across every step.
    pub fn total_tokens(&self) -> u64 {
        self.steps.iter().map(|s| s.tokens).sum()
    }
}

/// How a run ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// The agent finished with an answer.
    Finished { answer: String },
    /// The run stopped because a budget was exhausted.
    BudgetExhausted { reason: String },
    /// The run stopped on a fatal fault.
    Failed { reason: String },
    /// The run was interrupted (e.g. Ctrl-C); the partial trajectory is retained.
    Interrupted { reason: String },
}
