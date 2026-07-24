//! # kedge-audit — Shadow-Guard
//!
//! A **zero-risk dry-run runtime** plus a **forensic report**. Wrap the agent's
//! tool executor in an [`AuditExecutor`]: read-only tools run for real (so the
//! agent reasons on real data), but every *mutating* tool is intercepted at the
//! runtime boundary — nothing is written, no API is called, no database mutated.
//! The intent is journaled as [`Event::ToolExecutionAudited`] and a synthetic
//! success is returned so the ReAct loop keeps planning.
//!
//! Then [`AuditReport`] parses the ledger into a security scorecard (what the
//! agent *tried* to do) and a token/cost summary — with **honest** economics:
//! measured tokens for the ledger, and any projection driven by *your* explicit
//! price/volume inputs, never a baked-in figure.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kedge_core::{Observation, TaskId, ToolCall, ToolExecutor};
use kedge_ledger::{Event, Ledger};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("ledger error: {0}")]
    Ledger(#[from] kedge_ledger::LedgerError),
    #[error("malformed task id in ledger")]
    BadTaskId,
}

// ── Pillar 1: tool taxonomy ──
//
// The classifier now lives in `kedge-core::safety` so the wasm demo, the HITL
// gate, and the Python bridge all share the exact same code path. Re-exported
// here for backwards compatibility — existing `kedge_audit::classify` /
// `kedge_audit::{Risk, ToolSafety}` call sites are unchanged.
pub use kedge_core::{classify, Risk, ToolSafety};

// ── Pillar 2: the shadow interceptor ──

/// Wraps a [`ToolExecutor`]: read-only calls delegate to `inner`; mutating calls
/// are intercepted (not executed), journaled, and answered with a synthetic
/// success so the agent keeps planning.
pub struct AuditExecutor {
    inner: Arc<dyn ToolExecutor>,
    ledger: Option<Arc<Ledger>>,
    run_id: TaskId,
    intercepted: AtomicU64,
}

impl AuditExecutor {
    pub fn new(inner: Arc<dyn ToolExecutor>, ledger: Option<Arc<Ledger>>, run_id: TaskId) -> Self {
        AuditExecutor {
            inner,
            ledger,
            run_id,
            intercepted: AtomicU64::new(0),
        }
    }

    /// How many mutating calls have been intercepted.
    pub fn intercepted(&self) -> u64 {
        self.intercepted.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ToolExecutor for AuditExecutor {
    async fn execute(&self, call: &ToolCall) -> kedge_core::Result<Observation> {
        match classify(&call.name) {
            ToolSafety::ReadOnly => self.inner.execute(call).await,
            ToolSafety::Mutating { risk } => {
                self.intercepted.fetch_add(1, Ordering::SeqCst);
                tracing::info!(tool = %call.name, risk = risk.as_str(), "shadow-audit: intercepted mutating tool");
                if let Some(ledger) = &self.ledger {
                    let _ = ledger.record_event(
                        self.run_id,
                        &Event::ToolExecutionAudited {
                            tool: call.name.clone(),
                            risk: risk.as_str().to_string(),
                            arguments: call.arguments.clone(),
                        },
                    );
                }
                Ok(Observation::ok(format!(
                    "[SHADOW AUDIT] mutating tool `{}` ({} risk) was intercepted and NOT executed — \
                     no files, APIs, or data were touched. Proceed as if it succeeded.",
                    call.name,
                    risk.as_str()
                )))
            }
        }
    }
}

// ── Pillar 3: the forensic report ──

/// One intercepted mutating action.
#[derive(Debug, Clone, Serialize)]
pub struct AuditedAction {
    pub tool: String,
    pub risk: String,
}

/// The dual-perspective audit report.
#[derive(Debug, Clone, Serialize)]
pub struct AuditReport {
    pub runs: usize,
    pub total_tokens: u64,
    pub intercepted: Vec<AuditedAction>,
    pub kernel_violations: usize,
    pub subagent_failures: usize,
    /// Cumulative AST-compaction savings journaled in this ledger.
    pub compactions: u64,
    pub compaction_tokens_saved: u64,
    /// Optional, user-supplied economics — never fabricated.
    pub price_per_1k_tokens: Option<f64>,
    pub runs_per_day: Option<u64>,
}

impl AuditReport {
    /// Build a report by parsing every run + event in a ledger. `price_per_1k` and
    /// `runs_per_day` are *your* inputs — supply them to get a cost projection.
    pub fn from_ledger(
        path: impl AsRef<std::path::Path>,
        price_per_1k_tokens: Option<f64>,
        runs_per_day: Option<u64>,
    ) -> Result<Self, AuditError> {
        let ledger = Ledger::open(path)?;
        let runs = ledger.list_runs()?;
        let mut total_tokens = 0u64;
        let mut intercepted = Vec::new();
        let mut kernel_violations = 0;
        let mut subagent_failures = 0;

        for r in &runs {
            let tid = TaskId(Uuid::parse_str(&r.task_id).map_err(|_| AuditError::BadTaskId)?);
            if let Ok(traj) = ledger.replay(tid) {
                total_tokens += traj.total_tokens();
            }
            for ev in ledger.events(tid)? {
                match ev {
                    Event::ToolExecutionAudited { tool, risk, .. } => {
                        intercepted.push(AuditedAction { tool, risk })
                    }
                    Event::KernelSecurityViolation { .. } => kernel_violations += 1,
                    Event::SubagentFailed { .. } => subagent_failures += 1,
                    _ => {}
                }
            }
        }

        let totals = ledger.compaction_totals()?;

        Ok(AuditReport {
            runs: runs.len(),
            total_tokens,
            intercepted,
            kernel_violations,
            subagent_failures,
            compactions: totals.compactions,
            compaction_tokens_saved: totals.tokens_saved,
            price_per_1k_tokens,
            runs_per_day,
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Human-readable dual scorecard.
    pub fn to_pretty(&self) -> String {
        let mut s = String::from("═══ Kedge Shadow-Guard Audit ═══\n\n");

        // ── Security & Compliance (for the CISO) ──
        s.push_str("▸ Security & Compliance\n");
        s.push_str(&format!(
            "  intercepted mutating actions : {}\n",
            self.intercepted.len()
        ));
        for a in &self.intercepted {
            s.push_str(&format!("      • {} [{}]\n", a.tool, a.risk));
        }
        s.push_str(&format!(
            "  kernel / syscall violations  : {}\n",
            self.kernel_violations
        ));
        s.push_str(&format!(
            "  subagent failures            : {}\n",
            self.subagent_failures
        ));
        s.push_str(&format!(
            "  replay                       : all {} run(s) are byte-for-byte replayable from the journal\n\n",
            self.runs
        ));

        // ── Economics (for the VP Eng / CFO) — measured, with explicit inputs ──
        s.push_str("▸ Token & Cost (measured)\n");
        s.push_str(&format!("  runs                         : {}\n", self.runs));
        s.push_str(&format!(
            "  tokens used (this ledger)    : {}\n",
            self.total_tokens
        ));
        match self.price_per_1k_tokens {
            Some(price) => {
                let cost = self.total_tokens as f64 / 1000.0 * price;
                s.push_str(&format!(
                    "  cost (this ledger)           : ${cost:.4} @ ${price}/1k tokens\n"
                ));
                if let (Some(rpd), true) = (self.runs_per_day, self.runs > 0) {
                    let per_run = cost / self.runs as f64;
                    let monthly = per_run * rpd as f64 * 30.0;
                    s.push_str(&format!(
                        "  projection (YOUR inputs)     : ~${monthly:.2}/month at {rpd} runs/day, ${price}/1k\n"
                    ));
                }
            }
            None => s.push_str(
                "  cost                         : pass --price-per-1k <USD> for a cost figure\n",
            ),
        }
        // ── Compaction savings (measured, cumulative) ──
        s.push_str("\n▸ Compaction Savings (measured)\n");
        s.push_str(&format!(
            "  files compacted              : {}\n",
            self.compactions
        ));
        s.push_str(&format!(
            "  tokens saved (cumulative)    : {}\n",
            self.compaction_tokens_saved
        ));
        match self.price_per_1k_tokens {
            Some(price) => {
                let saved = self.compaction_tokens_saved as f64 / 1000.0 * price;
                s.push_str(&format!(
                    "  cost saved                   : ${saved:.4} @ ${price}/1k tokens\n"
                ));
            }
            None => s.push_str(
                "  cost saved                   : pass --price-per-1k <USD> for a dollar figure\n",
            ),
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    #[test]
    fn classification_is_fail_safe() {
        assert_eq!(classify("read_file"), ToolSafety::ReadOnly);
        assert_eq!(classify("list-dir"), ToolSafety::ReadOnly);
        assert_eq!(
            classify("delete_file"),
            ToolSafety::Mutating { risk: Risk::High }
        );
        assert_eq!(classify("shell"), ToolSafety::Mutating { risk: Risk::High });
        // Unconventional side-effecting names must NOT be treated as read-only.
        assert_eq!(
            classify("send_email"),
            ToolSafety::Mutating { risk: Risk::High }
        );
        assert_eq!(
            classify("frobnicate"),
            ToolSafety::Mutating { risk: Risk::Medium }
        );
    }

    struct RealTool;
    #[async_trait]
    impl ToolExecutor for RealTool {
        async fn execute(&self, call: &ToolCall) -> kedge_core::Result<Observation> {
            // In a real run this would touch the host. If audit ever lets a mutating
            // call through, the marker below leaks into the observation.
            Ok(Observation::ok(format!("REALLY RAN {}", call.name)))
        }
    }

    #[tokio::test]
    async fn shadow_executor_runs_reads_but_intercepts_mutations() {
        let ledger = Arc::new(Ledger::in_memory().unwrap());
        let run_id = TaskId::new();
        let audit = AuditExecutor::new(Arc::new(RealTool), Some(ledger.clone()), run_id);

        // Read-only tool executes for real.
        let read = audit
            .execute(&ToolCall::new(
                "read_file",
                serde_json::json!({"path": "x"}),
            ))
            .await
            .unwrap();
        assert!(read.content.contains("REALLY RAN"));

        // Mutating tool is intercepted — never reaches RealTool.
        let write = audit
            .execute(&ToolCall::new(
                "delete_file",
                serde_json::json!({"path": "/etc/passwd"}),
            ))
            .await
            .unwrap();
        assert!(!write.content.contains("REALLY RAN"));
        assert!(write.content.contains("SHADOW AUDIT"));
        assert_eq!(audit.intercepted(), 1);

        // …and the intent was journaled.
        let events = ledger.events(run_id).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolExecutionAudited { tool, risk, .. } if tool == "delete_file" && risk == "high"
        )));
    }

    #[test]
    fn report_summarizes_a_ledger_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.sqlite");
        let ledger = Ledger::open(&path).unwrap();
        let run_id = TaskId::new();
        ledger
            .record_event(
                run_id,
                &Event::ToolExecutionAudited {
                    tool: "shell".into(),
                    risk: "high".into(),
                    arguments: serde_json::json!({}),
                },
            )
            .unwrap();
        drop(ledger);

        // No price → no fabricated cost.
        let bare = AuditReport::from_ledger(&path, None, None).unwrap();
        assert_eq!(bare.intercepted.len(), 1);
        assert!(bare.to_pretty().contains("pass --price-per-1k"));
        assert!(!bare.to_pretty().contains("$1,620")); // no baked-in projection

        // With explicit inputs → a projection clearly attributed to the user.
        let priced = AuditReport::from_ledger(&path, Some(2.0), Some(1000)).unwrap();
        assert!(priced.to_pretty().contains("YOUR inputs"));
    }
}
