//! Error taxonomy for the harness.
//!
//! `sturdy-core` is the dependency root, so it can't depend on the satellite
//! crates. Their errors convert *into* [`HarnessError`] at the boundary (usually
//! via [`HarnessError::backend`]), keeping the core free of cycles.

use thiserror::Error;

/// Which budget dimension was blown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    /// Cumulative model tokens (prompt + completion).
    Tokens,
    /// Number of ReAct steps.
    Steps,
    /// Wall-clock deadline.
    WallClock,
}

impl std::fmt::Display for BudgetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BudgetKind::Tokens => write!(f, "token"),
            BudgetKind::Steps => write!(f, "step"),
            BudgetKind::WallClock => write!(f, "wall-clock"),
        }
    }
}

/// The single error type surfaced by the engine.
#[derive(Debug, Error)]
pub enum HarnessError {
    /// A hard budget ceiling was hit; the run is terminated deterministically.
    #[error("{kind} budget exceeded ({used}/{limit})")]
    BudgetExceeded {
        kind: BudgetKind,
        used: u64,
        limit: u64,
    },

    /// The state machine was asked to make a transition it does not allow.
    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition {
        from: &'static str,
        to: &'static str,
    },

    /// The reasoner (LLM policy) failed to produce a valid next action.
    #[error("reasoner failure: {0}")]
    Reasoner(String),

    /// A tool invocation failed. This is *recoverable*: it becomes an error
    /// observation the agent can react to, not a fatal harness error.
    #[error("tool `{tool}` failed: {message}")]
    Tool { tool: String, message: String },

    /// A satellite crate (mcp/exec/ledger/compact) reported a fatal error.
    #[error("{component} backend error: {message}")]
    Backend {
        component: &'static str,
        message: String,
    },

    /// The run was cancelled by an external signal or budget deadline.
    #[error("run cancelled")]
    Cancelled,

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("internal invariant violated: {0}")]
    Internal(String),
}

impl HarnessError {
    /// Wrap a satellite-crate error, tagging its originating component.
    pub fn backend(component: &'static str, err: impl std::fmt::Display) -> Self {
        HarnessError::Backend {
            component,
            message: err.to_string(),
        }
    }

    /// Construct a recoverable tool failure.
    pub fn tool(tool: impl Into<String>, message: impl std::fmt::Display) -> Self {
        HarnessError::Tool {
            tool: tool.into(),
            message: message.to_string(),
        }
    }

    /// Is this a budget exhaustion? Callers use this to distinguish a clean
    /// stop (budget) from a genuine fault.
    pub fn is_budget(&self) -> bool {
        matches!(self, HarnessError::BudgetExceeded { .. })
    }
}

/// Convenience alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, HarnessError>;
