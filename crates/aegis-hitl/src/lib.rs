//! # aegis-hitl — Human-in-the-Loop approval
//!
//! The third mode of the same interception boundary Aegis already has:
//!
//! * `--audit` dry-runs **everything** mutating (nothing executes),
//! * `aegis-policy` **blocks** disallowed tools outright,
//! * **HITL** pauses on the dangerous ones and **asks a human** — approve and it
//!   really runs, deny and the agent gets an error observation it can react to.
//!
//! [`ApprovalGate`] wraps any [`ToolExecutor`]: read-only tools pass straight
//! through (reusing `aegis-audit`'s fail-safe classifier), and mutating tools are
//! routed to an [`Approver`]. Every decision is journaled as
//! [`Event::ToolApprovalDecision`], so there is a permanent record of who let what
//! through. The default [`CliApprover`] prints a prompt and reads `y/N` from stdin.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aegis_audit::{classify, Risk, ToolSafety};
use sturdy_core::{Observation, TaskId, ToolCall, ToolExecutor};
use sturdy_ledger::{Event, Ledger};
use tokio::sync::oneshot;
use uuid::Uuid;

/// A human's decision on a pending high-risk tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

impl ApprovalDecision {
    pub fn approved(self) -> bool {
        matches!(self, ApprovalDecision::Approve)
    }
}

/// The source of approval decisions. Implement this for a webhook, a web UI, a
/// chat bot — anything that can answer "should this run?".
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, call: &ToolCall, risk: Risk) -> ApprovalDecision;
}

/// Prompts on the terminal and reads `y`/`N` from stdin.
pub struct CliApprover;

#[async_trait::async_trait]
impl Approver for CliApprover {
    async fn decide(&self, call: &ToolCall, risk: Risk) -> ApprovalDecision {
        let prompt =
            format!(
            "\n[Aegis HITL] Agent wants to execute `{}` ({} risk).\n  args: {}\n  Approve? [y/N] ",
            call.name, risk.as_str(), call.arguments
        );
        // stdin is blocking; keep it off the async runtime.
        let line = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            let mut s = String::new();
            let _ = std::io::stdin().read_line(&mut s);
            s
        })
        .await
        .unwrap_or_default();

        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ApprovalDecision::Approve,
            _ => ApprovalDecision::Deny,
        }
    }
}

/// Always approves — non-interactive default / test double.
pub struct AutoApprover;
#[async_trait::async_trait]
impl Approver for AutoApprover {
    async fn decide(&self, _call: &ToolCall, _risk: Risk) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

/// Always denies — a "read-only lockdown" / test double.
pub struct DenyingApprover;
#[async_trait::async_trait]
impl Approver for DenyingApprover {
    async fn decide(&self, _call: &ToolCall, _risk: Risk) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

// ── remote approval: a shared registry + a webhook-driven approver ──

/// A pending approval awaiting a human, as shown to a remote reviewer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingApproval {
    pub id: String,
    pub tool: String,
    pub risk: String,
}

struct Waiter {
    tool: String,
    risk: String,
    tx: oneshot::Sender<bool>,
}

/// A registry of in-flight approval requests, shared between the agent (which
/// awaits a decision inside [`WebhookApprover`]) and an external control plane
/// like `aegis serve` (which resolves it when a human clicks Approve/Deny).
#[derive(Default)]
pub struct PendingApprovals {
    waiters: Mutex<HashMap<String, Waiter>>,
}

impl PendingApprovals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a new pending approval; returns its id and a receiver that
    /// resolves once a human decides.
    pub fn register(&self, tool: &str, risk: &str) -> (String, oneshot::Receiver<bool>) {
        let id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(
            id.clone(),
            Waiter {
                tool: tool.to_string(),
                risk: risk.to_string(),
                tx,
            },
        );
        (id, rx)
    }

    /// Resolve a pending approval by id. Returns `true` if it was actually
    /// pending (so the control plane can 404 on an unknown/expired id).
    pub fn resolve(&self, id: &str, approved: bool) -> bool {
        match self.waiters.lock().unwrap().remove(id) {
            Some(w) => {
                let _ = w.tx.send(approved);
                true
            }
            None => false,
        }
    }

    /// Drop a pending approval without resolving it (e.g. on timeout).
    pub fn cancel(&self, id: &str) {
        self.waiters.lock().unwrap().remove(id);
    }

    /// A snapshot of everything currently awaiting a human.
    pub fn list(&self) -> Vec<PendingApproval> {
        self.waiters
            .lock()
            .unwrap()
            .iter()
            .map(|(id, w)| PendingApproval {
                id: id.clone(),
                tool: w.tool.clone(),
                risk: w.risk.clone(),
            })
            .collect()
    }
}

/// Approves out-of-band via a human, for production/server use where no terminal
/// is attached. Registers the request in a shared [`PendingApprovals`], best-effort
/// notifies an external system over HTTP (Slack, a dashboard), then awaits the
/// human's decision — resolved by `aegis serve`'s approve endpoint. **Denies on
/// timeout** (fail-safe).
pub struct WebhookApprover {
    http: reqwest::Client,
    notify_url: Option<String>,
    approvals: Arc<PendingApprovals>,
    timeout: Duration,
}

impl WebhookApprover {
    pub fn new(
        approvals: Arc<PendingApprovals>,
        notify_url: Option<String>,
        timeout: Duration,
    ) -> Self {
        WebhookApprover {
            http: reqwest::Client::new(),
            notify_url,
            approvals,
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl Approver for WebhookApprover {
    async fn decide(&self, call: &ToolCall, risk: Risk) -> ApprovalDecision {
        let (id, rx) = self.approvals.register(&call.name, risk.as_str());

        // Best-effort notification — the human resolves via the control-plane API.
        if let Some(url) = &self.notify_url {
            let payload = serde_json::json!({
                "approval_id": id,
                "tool": call.name,
                "risk": risk.as_str(),
                "arguments": call.arguments,
            });
            let _ = self.http.post(url).json(&payload).send().await;
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(true)) => ApprovalDecision::Approve,
            Ok(Ok(false)) => ApprovalDecision::Deny,
            // Timeout or the sender dropped → deny (fail-safe).
            _ => {
                self.approvals.cancel(&id);
                ApprovalDecision::Deny
            }
        }
    }
}

/// Wraps a [`ToolExecutor`]: read-only tools pass through; mutating tools require
/// a human's `Approve` before they execute for real, and every decision is
/// journaled.
pub struct ApprovalGate {
    inner: Arc<dyn ToolExecutor>,
    approver: Arc<dyn Approver>,
    ledger: Option<Arc<Ledger>>,
    run_id: TaskId,
    denied: AtomicU64,
}

impl ApprovalGate {
    pub fn new(
        inner: Arc<dyn ToolExecutor>,
        approver: Arc<dyn Approver>,
        ledger: Option<Arc<Ledger>>,
        run_id: TaskId,
    ) -> Self {
        ApprovalGate {
            inner,
            approver,
            ledger,
            run_id,
            denied: AtomicU64::new(0),
        }
    }

    /// How many tool calls a human has denied.
    pub fn denied(&self) -> u64 {
        self.denied.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ToolExecutor for ApprovalGate {
    async fn execute(&self, call: &ToolCall) -> sturdy_core::Result<Observation> {
        let risk = match classify(&call.name) {
            ToolSafety::ReadOnly => return self.inner.execute(call).await,
            ToolSafety::Mutating { risk } => risk,
        };

        let decision = self.approver.decide(call, risk).await;
        let approved = decision.approved();
        tracing::info!(tool = %call.name, risk = risk.as_str(), approved, "HITL approval decision");

        if let Some(ledger) = &self.ledger {
            let _ = ledger.record_event(
                self.run_id,
                &Event::ToolApprovalDecision {
                    tool: call.name.clone(),
                    risk: risk.as_str().to_string(),
                    approved,
                    arguments: call.arguments.clone(),
                },
            );
        }

        if approved {
            self.inner.execute(call).await
        } else {
            self.denied.fetch_add(1, Ordering::SeqCst);
            Ok(Observation::error(format!(
                "tool `{}` was denied by a human reviewer (HITL). Choose a different approach.",
                call.name
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct RealTool;
    #[async_trait]
    impl ToolExecutor for RealTool {
        async fn execute(&self, call: &ToolCall) -> sturdy_core::Result<Observation> {
            Ok(Observation::ok(format!("RAN {}", call.name)))
        }
    }

    fn gate(approver: Arc<dyn Approver>, ledger: Arc<Ledger>) -> (ApprovalGate, TaskId) {
        let run_id = TaskId::new();
        (
            ApprovalGate::new(Arc::new(RealTool), approver, Some(ledger), run_id),
            run_id,
        )
    }

    #[tokio::test]
    async fn read_only_tools_bypass_approval() {
        let ledger = Arc::new(Ledger::in_memory().unwrap());
        let (g, run_id) = gate(Arc::new(DenyingApprover), ledger.clone());
        // Even with a denying approver, a read tool runs and records no decision.
        let obs = g
            .execute(&ToolCall::new("read_file", serde_json::json!({})))
            .await
            .unwrap();
        assert!(obs.content.contains("RAN"));
        assert!(ledger.events(run_id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn approved_mutation_runs_and_is_journaled() {
        let ledger = Arc::new(Ledger::in_memory().unwrap());
        let (g, run_id) = gate(Arc::new(AutoApprover), ledger.clone());
        let obs = g
            .execute(&ToolCall::new("delete_file", serde_json::json!({"p": "x"})))
            .await
            .unwrap();
        assert!(obs.content.contains("RAN")); // really executed after approval
        let events = ledger.events(run_id).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolApprovalDecision { tool, approved, .. } if tool == "delete_file" && *approved
        )));
    }

    #[tokio::test]
    async fn denied_mutation_is_blocked_and_journaled() {
        let ledger = Arc::new(Ledger::in_memory().unwrap());
        let (g, run_id) = gate(Arc::new(DenyingApprover), ledger.clone());
        let obs = g
            .execute(&ToolCall::new("prod_db_migrate", serde_json::json!({})))
            .await
            .unwrap();
        assert!(obs.is_error);
        assert!(!obs.content.contains("RAN")); // never reached the real tool
        assert!(obs.content.contains("denied by a human"));
        assert_eq!(g.denied(), 1);
        let events = ledger.events(run_id).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolApprovalDecision { tool, approved, .. } if tool == "prod_db_migrate" && !*approved
        )));
    }

    #[tokio::test]
    async fn webhook_approver_resolves_via_the_shared_registry() {
        let approvals = PendingApprovals::new();
        let approver = WebhookApprover::new(approvals.clone(), None, Duration::from_secs(5));

        // The agent awaits a remote decision…
        let decide = tokio::spawn(async move {
            approver
                .decide(
                    &ToolCall::new("delete_file", serde_json::json!({})),
                    Risk::High,
                )
                .await
        });

        // …a human (via `aegis serve`) approves the pending request.
        let id = loop {
            if let Some(p) = approvals.list().into_iter().next() {
                break p.id;
            }
            tokio::task::yield_now().await;
        };
        assert!(approvals.resolve(&id, true));
        assert_eq!(decide.await.unwrap(), ApprovalDecision::Approve);
    }

    #[tokio::test]
    async fn webhook_approver_denies_on_timeout() {
        let approvals = PendingApprovals::new();
        let approver = WebhookApprover::new(approvals, None, Duration::from_millis(50));
        let d = approver
            .decide(&ToolCall::new("shell", serde_json::json!({})), Risk::High)
            .await;
        assert_eq!(d, ApprovalDecision::Deny);
    }
}
