//! Tool-safety classification — the taxonomy Shadow-Guard uses to decide what an
//! agent is allowed to execute for real.
//!
//! This lives in `kedge-core` (not `kedge-audit`) deliberately: it is pure,
//! dependency-free logic depended on by the audit executor, the HITL gate, the
//! Python bridge, *and* the WebAssembly demo. Keeping it in the wasm-clean core
//! lets every one of those use the exact same classifier — the browser demo runs
//! the real code path, not a copy.

use serde::Serialize;

/// How dangerous a mutating tool is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Medium,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

/// A tool's safety boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSafety {
    /// Safe to execute for real (reads, queries, compaction).
    ReadOnly,
    /// Has side effects — intercepted in audit mode.
    Mutating { risk: Risk },
}

impl ToolSafety {
    pub fn is_mutating(self) -> bool {
        matches!(self, ToolSafety::Mutating { .. })
    }
}

/// Verbs whose tools only read state — these run for real even in audit mode.
const READ_VERBS: &[&str] = &[
    "read",
    "get",
    "list",
    "search",
    "find",
    "query",
    "fetch",
    "show",
    "cat",
    "grep",
    "view",
    "describe",
    "inspect",
    "compact",
    "outline",
    "ls",
    "stat",
    "head",
    "tail",
    "count",
    "diff",
    "status",
    "log",
    "help",
    "summarize",
    "analyze",
    "lookup",
    "check",
];

/// Explicitly dangerous verbs.
const HIGH_RISK_VERBS: &[&str] = &[
    "exec",
    "execute",
    "shell",
    "run",
    "delete",
    "rm",
    "drop",
    "kill",
    "destroy",
    "remove",
    "sudo",
    "chmod",
    "chown",
    "format",
    "truncate",
    "overwrite",
    "deploy",
    "publish",
    "send",
    "post",
    "charge",
    "transfer",
    "pay",
];

/// Classify a tool by its name. **Fail-safe:** anything not recognized as clearly
/// read-only is treated as mutating, so an oddly-named side-effecting tool (e.g.
/// `send_email`, `charge_card`) is never executed for real in audit mode.
pub fn classify(tool_name: &str) -> ToolSafety {
    let head = tool_name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_ascii_lowercase();

    if READ_VERBS.contains(&head.as_str()) {
        ToolSafety::ReadOnly
    } else if HIGH_RISK_VERBS.contains(&head.as_str()) {
        ToolSafety::Mutating { risk: Risk::High }
    } else {
        // Unknown verb → assume it can mutate. Safety over convenience.
        ToolSafety::Mutating { risk: Risk::Medium }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_verbs_are_read_only() {
        for v in ["read_file", "list-dir", "grep", "search_code", "compact"] {
            assert_eq!(classify(v), ToolSafety::ReadOnly, "{v} should be read-only");
        }
    }

    #[test]
    fn dangerous_verbs_are_high_risk() {
        for v in ["rm", "shell", "delete_all", "sudo_thing", "charge_card"] {
            assert_eq!(
                classify(v),
                ToolSafety::Mutating { risk: Risk::High },
                "{v} should be high-risk"
            );
        }
    }

    #[test]
    fn unknown_verbs_fail_safe_to_mutating() {
        // The whole safety argument: an unrecognized tool is assumed dangerous.
        assert_eq!(
            classify("frobnicate_the_database"),
            ToolSafety::Mutating { risk: Risk::Medium }
        );
        assert_eq!(classify(""), ToolSafety::Mutating { risk: Risk::Medium });
    }
}
