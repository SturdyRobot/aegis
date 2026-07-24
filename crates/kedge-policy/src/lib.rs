//! # kedge-policy
//!
//! Lightweight, user-space guardrails for an agent run — the *opposite* of pulling
//! in a full OPA/Rego engine (heavy WASM/C bindings, against Kedge's ethos). A
//! small, native Rust matcher over an `kedge-policy.toml`:
//!
//! ```toml
//! blocked_tools      = ["shell", "delete_file"]
//! pii_redaction      = ['\b\d{3}-\d{2}-\d{4}\b', '[\w.+-]+@[\w-]+\.[\w.-]+']
//! max_tokens_per_run = 50000
//! max_steps_per_run  = 20
//! ```
//!
//! Wrap any [`ToolExecutor`] in a [`PolicyGuard`] to block disallowed tools and
//! redact PII from tool output before it ever reaches the model or the ledger.
//! This is the portable user-space layer; it complements `kedge-probe` (kernel
//! enforcement) without needing privileges.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kedge_core::{Budget, Observation, ToolCall, ToolExecutor};
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("reading policy file: {0}")]
    Io(#[from] std::io::Error),
    #[error("parsing policy TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid PII redaction regex `{pattern}`: {source}")]
    Regex {
        pattern: String,
        source: regex::Error,
    },
}

/// The raw TOML shape.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyConfig {
    #[serde(default)]
    blocked_tools: Vec<String>,
    #[serde(default)]
    pii_redaction: Vec<String>,
    #[serde(default)]
    max_tokens_per_run: Option<u64>,
    #[serde(default)]
    max_steps_per_run: Option<u64>,
}

/// A compiled, ready-to-enforce policy.
#[derive(Clone, Debug)]
pub struct Policy {
    blocked: HashSet<String>,
    pii: Vec<Regex>,
    max_tokens: Option<u64>,
    max_steps: Option<u64>,
}

impl Policy {
    /// Parse and compile a policy from TOML text.
    pub fn from_toml_str(s: &str) -> Result<Self, PolicyError> {
        Self::compile(toml::from_str(s)?)
    }

    /// Parse and compile a policy from an `kedge-policy.toml` file.
    pub fn from_toml_file(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        Self::from_toml_str(&std::fs::read_to_string(path)?)
    }

    fn compile(cfg: PolicyConfig) -> Result<Self, PolicyError> {
        let mut pii = Vec::with_capacity(cfg.pii_redaction.len());
        for pattern in &cfg.pii_redaction {
            pii.push(Regex::new(pattern).map_err(|source| PolicyError::Regex {
                pattern: pattern.clone(),
                source,
            })?);
        }
        Ok(Policy {
            blocked: cfg.blocked_tools.into_iter().collect(),
            pii,
            max_tokens: cfg.max_tokens_per_run,
            max_steps: cfg.max_steps_per_run,
        })
    }

    /// Whether `tool` is permitted.
    pub fn allows_tool(&self, tool: &str) -> bool {
        !self.blocked.contains(tool)
    }

    /// Replace every configured PII pattern with `[REDACTED]`.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for re in &self.pii {
            out = re.replace_all(&out, "[REDACTED]").into_owned();
        }
        out
    }

    /// A [`Budget`] reflecting the policy's per-run ceilings (defaults where unset).
    pub fn budget(&self) -> Budget {
        Budget {
            max_tokens: self.max_tokens.unwrap_or(100_000),
            max_steps: self.max_steps.unwrap_or(30),
            wall_clock: Duration::from_secs(300),
        }
    }
}

/// Wraps a [`ToolExecutor`], enforcing the policy: blocked tools are refused with
/// an error observation (the agent can react but can't run them), and PII in tool
/// output is redacted before the agent or ledger ever sees it.
pub struct PolicyGuard {
    policy: Arc<Policy>,
    inner: Arc<dyn ToolExecutor>,
}

impl PolicyGuard {
    pub fn new(policy: Arc<Policy>, inner: Arc<dyn ToolExecutor>) -> Self {
        PolicyGuard { policy, inner }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for PolicyGuard {
    async fn execute(&self, call: &ToolCall) -> kedge_core::Result<Observation> {
        if !self.policy.allows_tool(&call.name) {
            tracing::warn!(tool = %call.name, "tool blocked by policy");
            return Ok(Observation::error(format!(
                "tool `{}` is blocked by policy",
                call.name
            )));
        }
        let obs = self.inner.execute(call).await?;
        Ok(Observation {
            content: self.policy.redact(&obs.content),
            is_error: obs.is_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = r#"
        blocked_tools      = ["shell", "delete_file"]
        pii_redaction      = ['\b\d{3}-\d{2}-\d{4}\b', '[\w.+-]+@[\w-]+\.[\w.-]+']
        max_tokens_per_run = 50000
        max_steps_per_run  = 20
    "#;

    #[test]
    fn parses_and_enforces_the_matcher() {
        let p = Policy::from_toml_str(TOML).unwrap();
        assert!(!p.allows_tool("shell"));
        assert!(!p.allows_tool("delete_file"));
        assert!(p.allows_tool("search"));
        assert_eq!(p.budget().max_tokens, 50_000);
        assert_eq!(p.budget().max_steps, 20);
    }

    #[test]
    fn redacts_pii() {
        let p = Policy::from_toml_str(TOML).unwrap();
        let redacted = p.redact("SSN 123-45-6789 email bob@acme.io ok");
        assert!(!redacted.contains("123-45-6789"));
        assert!(!redacted.contains("bob@acme.io"));
        assert_eq!(redacted.matches("[REDACTED]").count(), 2);
    }

    #[test]
    fn bad_regex_is_a_clear_error() {
        let err = Policy::from_toml_str("pii_redaction = ['(unclosed']").unwrap_err();
        assert!(matches!(err, PolicyError::Regex { .. }));
    }

    #[tokio::test]
    async fn guard_blocks_tools_and_redacts_output() {
        use async_trait::async_trait;

        struct LeakyTool;
        #[async_trait]
        impl ToolExecutor for LeakyTool {
            async fn execute(&self, call: &ToolCall) -> kedge_core::Result<Observation> {
                Ok(Observation::ok(format!(
                    "ran {}; contact 555-12-3456",
                    call.name
                )))
            }
        }

        let policy = Arc::new(Policy::from_toml_str(TOML).unwrap());
        let guard = PolicyGuard::new(policy, Arc::new(LeakyTool));

        // A blocked tool never reaches the inner executor.
        let blocked = guard
            .execute(&ToolCall::new("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(blocked.is_error);
        assert!(blocked.content.contains("blocked by policy"));

        // An allowed tool runs, but its PII output is redacted.
        let allowed = guard
            .execute(&ToolCall::new("search", serde_json::json!({})))
            .await
            .unwrap();
        assert!(!allowed.is_error);
        assert!(!allowed.content.contains("555-12-3456"));
        assert!(allowed.content.contains("[REDACTED]"));
    }
}
