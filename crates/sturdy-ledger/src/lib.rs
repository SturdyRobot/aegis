//! # sturdy-ledger
//!
//! An append-only SQLite journal of every run. It serves two jobs:
//!
//! 1. **Audit** — as the engine executes, each step is written synchronously via
//!    the [`StepObserver`] hook, so a crashed run still leaves a complete trail.
//! 2. **Deterministic replay** — a finished run can be reconstructed
//!    byte-for-byte from the journal ([`Ledger::replay`]), which is the basis for
//!    debugging, regression fixtures, and cache-backed re-execution.
//!
//! The connection lives behind an `Arc<Mutex<_>>` so a single ledger can be both
//! queried directly and handed to the engine as an observer.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use sturdy_core::{HarnessError, Outcome, Step, StepObserver, Task, TaskId, Trajectory};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no run found for task {0}")]
    NotFound(TaskId),
    #[error("ledger lock poisoned")]
    Poisoned,
}

impl From<LedgerError> for HarnessError {
    fn from(e: LedgerError) -> Self {
        HarnessError::backend("ledger", e)
    }
}

pub type Result<T> = std::result::Result<T, LedgerError>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    task_id     TEXT PRIMARY KEY,
    goal        TEXT NOT NULL,
    workspace   TEXT,
    started_ms  INTEGER NOT NULL,
    ended_ms    INTEGER,
    status      TEXT,          -- 'running' | 'finished' | 'budget_exhausted' | 'failed'
    answer      TEXT
);
CREATE TABLE IF NOT EXISTS steps (
    task_id     TEXT NOT NULL,
    idx         INTEGER NOT NULL,
    thought     TEXT NOT NULL,
    action      TEXT NOT NULL,  -- JSON-encoded core::Action
    observation TEXT,           -- JSON-encoded Option<core::Observation>
    tokens      INTEGER NOT NULL,
    elapsed_ms  INTEGER NOT NULL,
    PRIMARY KEY (task_id, idx),
    FOREIGN KEY (task_id) REFERENCES runs(task_id)
);
CREATE TABLE IF NOT EXISTS events (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id   TEXT NOT NULL,
    ts_ms     INTEGER NOT NULL,
    category  TEXT NOT NULL,  -- e.g. 'mcp_tool_execution'
    payload   TEXT NOT NULL,  -- JSON-encoded Event
    FOREIGN KEY (task_id) REFERENCES runs(task_id)
);
-- Standalone AST-compaction savings, journaled here so `aegis audit` can report
-- a cumulative "tokens saved" figure. Not tied to a run (there is no task_id).
CREATE TABLE IF NOT EXISTS compactions (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms            INTEGER NOT NULL,
    label            TEXT,           -- e.g. the file path that was compacted
    original_tokens  INTEGER NOT NULL,
    compacted_tokens INTEGER NOT NULL
);
"#;

/// A categorized, out-of-band event journaled alongside the step trail. Steps are
/// the ReAct backbone; events capture side-channel activity worth auditing on its
/// own — currently tool calls proxied through external MCP servers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum Event {
    /// A tool call routed to an external MCP server.
    McpToolExecution {
        server: String,
        tool: String,
        arguments: serde_json::Value,
        output: String,
        is_error: bool,
    },
    /// A kernel-boundary security violation reported by `aegis-probe`'s eBPF
    /// supervisor — e.g. a blocked `execve`, `connect`, or write to a protected path.
    KernelSecurityViolation {
        pid: u32,
        tgid: u32,
        /// The violated boundary: "exec" | "connect" | "write".
        /// (Named `boundary`, not `kind`, to avoid clashing with the serde tag.)
        boundary: String,
        /// What was attempted (the exec path, destination address, or file path).
        detail: String,
    },
    /// A supervised subagent (spawned via `aegis-mesh`) terminated abnormally —
    /// panic, timeout, cancellation, or budget exhaustion.
    SubagentFailed {
        name: String,
        reason: String,
        tokens_used: u64,
    },
    /// A mutating tool call intercepted in shadow/audit mode: the agent *intended*
    /// to run it, but Aegis skipped physical execution (nothing was touched).
    ToolExecutionAudited {
        tool: String,
        /// Risk classification: "medium" | "high".
        risk: String,
        /// The arguments the agent intended to pass.
        arguments: serde_json::Value,
    },
    /// A human-in-the-loop approval decision for a high-risk tool call: the agent
    /// paused, a human was asked, and this records what they decided.
    ToolApprovalDecision {
        tool: String,
        /// Risk classification: "medium" | "high".
        risk: String,
        approved: bool,
        /// The arguments the agent intended to pass.
        arguments: serde_json::Value,
    },
}

impl Event {
    /// A short, stable category label for the `category` column and queries.
    pub fn category(&self) -> &'static str {
        match self {
            Event::McpToolExecution { .. } => "mcp_tool_execution",
            Event::KernelSecurityViolation { .. } => "kernel_security_violation",
            Event::SubagentFailed { .. } => "subagent_failed",
            Event::ToolExecutionAudited { .. } => "tool_execution_audited",
            Event::ToolApprovalDecision { .. } => "tool_approval_decision",
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The SQLite-backed journal.
#[derive(Clone)]
pub struct Ledger {
    conn: Arc<Mutex<Connection>>,
}

impl Ledger {
    /// Open (or create) a journal at `path`, applying the schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An ephemeral in-memory journal (used in tests and dry runs).
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL is a no-op for `:memory:` (harmless); on a file DB a failure is
        // worth knowing about rather than swallowing.
        if let Err(e) = conn.pragma_update(None, "journal_mode", "WAL") {
            tracing::warn!(error = %e, "could not enable WAL journal mode");
        }
        // Without a busy timeout a concurrent writer (a second agent, or
        // `aegis serve` sharing the file) fails instantly with SQLITE_BUSY
        // instead of waiting for the lock.
        conn.pragma_update(None, "busy_timeout", 5_000).ok();
        // The standard companion to WAL: still crash-safe, far fewer fsyncs.
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        conn.execute_batch(SCHEMA)?;
        Ok(Ledger {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| LedgerError::Poisoned)
    }

    /// Record the start of a run. Idempotent (`INSERT OR REPLACE`).
    #[tracing::instrument(name = "ledger.op", skip_all, fields(event_type = "begin_run", run_id = %task.id))]
    pub fn begin_run(&self, task: &Task) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO runs (task_id, goal, workspace, started_ms, status)
             VALUES (?1, ?2, ?3, ?4, 'running')",
            rusqlite::params![task.id.to_string(), task.goal, task.workspace, now_ms()],
        )?;
        Ok(())
    }

    /// Append one finalized step.
    ///
    /// If `begin_run` was never called for this task (e.g. an observer wired up
    /// without it), a placeholder run row is created so the foreign key holds and
    /// the step is never silently lost — the "crashed run still leaves a trail"
    /// guarantee. A later `begin_run` overwrites the placeholder.
    #[tracing::instrument(name = "ledger.op", skip_all, fields(event_type = "record_step", run_id = %task_id, step_id = step.index))]
    pub fn record_step(&self, task_id: TaskId, step: &Step) -> Result<()> {
        let action = serde_json::to_string(&step.action)?;
        let observation = match &step.observation {
            Some(o) => Some(serde_json::to_string(o)?),
            None => None,
        };
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO runs (task_id, goal, started_ms, status)
             VALUES (?1, '(recovered)', ?2, 'running')",
            rusqlite::params![task_id.to_string(), now_ms()],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO steps
             (task_id, idx, thought, action, observation, tokens, elapsed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                task_id.to_string(),
                step.index,
                step.thought.0,
                action,
                observation,
                step.tokens,
                step.elapsed_ms,
            ],
        )?;
        Ok(())
    }

    /// Mark a run complete, recording its terminal status and answer.
    #[tracing::instrument(name = "ledger.op", skip_all, fields(event_type = "finalize", run_id = %task_id))]
    pub fn finalize(&self, task_id: TaskId, outcome: &Outcome) -> Result<()> {
        let (status, answer) = match outcome {
            Outcome::Finished { answer } => ("finished", Some(answer.clone())),
            Outcome::BudgetExhausted { reason } => ("budget_exhausted", Some(reason.clone())),
            Outcome::Failed { reason } => ("failed", Some(reason.clone())),
            Outcome::Interrupted { reason } => ("interrupted", Some(reason.clone())),
        };
        let conn = self.lock()?;
        let affected = conn.execute(
            "UPDATE runs SET status = ?2, answer = ?3, ended_ms = ?4 WHERE task_id = ?1",
            rusqlite::params![task_id.to_string(), status, answer, now_ms()],
        )?;
        if affected == 0 {
            return Err(LedgerError::NotFound(task_id));
        }
        Ok(())
    }

    /// Append a categorized [`Event`] (e.g. an MCP tool execution). Creates a
    /// placeholder run row if needed so the event is never silently lost.
    #[tracing::instrument(name = "ledger.op", skip_all, fields(event_type = "record_event", run_id = %task_id, category = event.category()))]
    pub fn record_event(&self, task_id: TaskId, event: &Event) -> Result<()> {
        let payload = serde_json::to_string(event)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO runs (task_id, goal, started_ms, status)
             VALUES (?1, '', ?2, 'running')",
            rusqlite::params![task_id.to_string(), now_ms()],
        )?;
        conn.execute(
            "INSERT INTO events (task_id, ts_ms, category, payload) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![task_id.to_string(), now_ms(), event.category(), payload],
        )?;
        Ok(())
    }

    /// Journal one AST-compaction's token savings (not tied to a run). Best-effort
    /// telemetry: callers may ignore the error and still return their result.
    pub fn record_compaction(
        &self,
        original_tokens: u64,
        compacted_tokens: u64,
        label: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO compactions (ts_ms, label, original_tokens, compacted_tokens)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![now_ms(), label, original_tokens, compacted_tokens],
        )?;
        Ok(())
    }

    /// Cumulative compaction savings across every entry in this ledger.
    pub fn compaction_totals(&self) -> Result<CompactionTotals> {
        let conn = self.lock()?;
        let (count, original, compacted): (u64, u64, u64) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(original_tokens), 0),
                    COALESCE(SUM(compacted_tokens), 0)
             FROM compactions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(CompactionTotals {
            compactions: count,
            original_tokens: original,
            compacted_tokens: compacted,
            tokens_saved: original.saturating_sub(compacted),
        })
    }

    /// Every event recorded for a run, in insertion order.
    pub fn events(&self, task_id: TaskId) -> Result<Vec<Event>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT payload FROM events WHERE task_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([task_id.to_string()], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    /// Reconstruct a trajectory from the journal, in step order.
    pub fn replay(&self, task_id: TaskId) -> Result<Trajectory> {
        let conn = self.lock()?;
        // Confirm the run exists so we can distinguish "empty" from "unknown".
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM runs WHERE task_id = ?1",
                [task_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(LedgerError::NotFound(task_id));
        }

        let mut stmt = conn.prepare(
            "SELECT idx, thought, action, observation, tokens, elapsed_ms
             FROM steps WHERE task_id = ?1 ORDER BY idx ASC",
        )?;
        let rows = stmt.query_map([task_id.to_string()], |row| {
            Ok(RawStep {
                index: row.get(0)?,
                thought: row.get(1)?,
                action: row.get(2)?,
                observation: row.get(3)?,
                tokens: row.get(4)?,
                elapsed_ms: row.get(5)?,
            })
        })?;

        let mut trajectory = Trajectory::new(task_id);
        for row in rows {
            trajectory.push(row?.into_step()?);
        }
        Ok(trajectory)
    }

    /// Every task id in the journal, newest first.
    pub fn list_runs(&self) -> Result<Vec<RunSummary>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, goal, status, started_ms FROM runs ORDER BY started_ms DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RunSummary {
                task_id: row.get::<_, String>(0)?,
                goal: row.get(1)?,
                status: row.get(2)?,
                started_ms: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Full metadata for a single run.
    pub fn run_detail(&self, task_id: TaskId) -> Result<RunDetail> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT task_id, goal, workspace, status, answer, started_ms, ended_ms
             FROM runs WHERE task_id = ?1",
            [task_id.to_string()],
            |r| {
                Ok(RunDetail {
                    task_id: r.get(0)?,
                    goal: r.get(1)?,
                    workspace: r.get(2)?,
                    status: r.get(3)?,
                    answer: r.get(4)?,
                    started_ms: r.get(5)?,
                    ended_ms: r.get(6)?,
                })
            },
        )
        .optional()?
        .ok_or(LedgerError::NotFound(task_id))
    }

    /// A cloneable observer that journals steps live as the engine runs.
    pub fn observer(&self) -> Arc<LedgerObserver> {
        Arc::new(LedgerObserver {
            ledger: self.clone(),
        })
    }
}

/// A row as read back from SQLite, before JSON decode.
struct RawStep {
    index: u32,
    thought: String,
    action: String,
    observation: Option<String>,
    tokens: u64,
    elapsed_ms: u64,
}

impl RawStep {
    fn into_step(self) -> Result<Step> {
        let action = serde_json::from_str(&self.action)?;
        let observation = match self.observation {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        };
        Ok(Step {
            index: self.index,
            thought: sturdy_core::Thought(self.thought),
            action,
            observation,
            tokens: self.tokens,
            elapsed_ms: self.elapsed_ms,
        })
    }
}

/// Cumulative AST-compaction savings across a ledger.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CompactionTotals {
    pub compactions: u64,
    pub original_tokens: u64,
    pub compacted_tokens: u64,
    pub tokens_saved: u64,
}

/// Lightweight run listing for the CLI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunSummary {
    pub task_id: String,
    pub goal: String,
    pub status: Option<String>,
    pub started_ms: i64,
}

/// Full metadata for a single run (for `ledger show`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunDetail {
    pub task_id: String,
    pub goal: String,
    pub workspace: Option<String>,
    pub status: Option<String>,
    pub answer: Option<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

/// Adapts a [`Ledger`] to the engine's [`StepObserver`] hook. Errors are logged
/// rather than propagated (the observer contract is infallible), but a failed
/// write never corrupts the run.
pub struct LedgerObserver {
    ledger: Ledger,
}

impl StepObserver for LedgerObserver {
    /// DESIGN NOTE — this performs a blocking SQLite write from inside the async
    /// ReAct loop, and that is deliberate.
    ///
    /// Moving the write to a background task/thread would stop it occupying a
    /// Tokio worker, but it would also make journaling *asynchronous* with
    /// respect to the run: `replay()` and `resume()` are called immediately
    /// after a run (including on the Ctrl-C path) and depend on every completed
    /// step already being durable. Buffering would trade a sub-millisecond
    /// block for a lost-step window on crash — the exact failure this ledger
    /// exists to prevent.
    ///
    /// The write is a single-row insert against a WAL database with a busy
    /// timeout, so the block is short and bounded. If a future workload proves
    /// this is a bottleneck (many agents sharing one ledger under `aegis-mesh`),
    /// the fix is a per-agent ledger or a durable write-ahead queue — not a
    /// naive fire-and-forget channel.
    fn on_step(&self, task: &Task, step: &Step) {
        if let Err(e) = self.ledger.record_step(task.id, step) {
            tracing::error!(task = %task.id, index = step.index, error = %e, "ledger write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sturdy_core::{Action, Observation, Thought, ToolCall};

    fn sample_steps() -> Vec<Step> {
        vec![
            Step {
                index: 0,
                thought: Thought("inspect the tree".into()),
                action: Action::Tool(ToolCall::new(
                    "read_file",
                    serde_json::json!({"path":"a.rs"}),
                )),
                observation: Some(Observation::ok("fn main() {}")),
                tokens: 42,
                elapsed_ms: 12,
            },
            Step {
                index: 1,
                thought: Thought("done".into()),
                action: Action::Finish {
                    answer: "ok".into(),
                },
                observation: None,
                tokens: 7,
                elapsed_ms: 3,
            },
        ]
    }

    #[test]
    fn replay_is_byte_identical_to_what_was_written() {
        let ledger = Ledger::in_memory().unwrap();
        let task = Task::new("do a thing");
        ledger.begin_run(&task).unwrap();

        let steps = sample_steps();
        for s in &steps {
            ledger.record_step(task.id, s).unwrap();
        }
        ledger
            .finalize(
                task.id,
                &Outcome::Finished {
                    answer: "ok".into(),
                },
            )
            .unwrap();

        let replayed = ledger.replay(task.id).unwrap();
        assert_eq!(replayed.len(), 2);

        // Deterministic replay ⇒ identical JSON round-trip.
        let want = serde_json::to_value(&steps).unwrap();
        let got = serde_json::to_value(&replayed.steps).unwrap();
        assert_eq!(want, got);
    }

    #[test]
    fn replay_of_unknown_run_is_not_found() {
        let ledger = Ledger::in_memory().unwrap();
        let err = ledger.replay(TaskId::new()).unwrap_err();
        assert!(matches!(err, LedgerError::NotFound(_)));
    }

    #[test]
    fn observer_journals_live() {
        let ledger = Ledger::in_memory().unwrap();
        let task = Task::new("observe me");
        ledger.begin_run(&task).unwrap();

        let observer = ledger.observer();
        for s in &sample_steps() {
            observer.on_step(&task, s);
        }
        assert_eq!(ledger.replay(task.id).unwrap().len(), 2);
    }

    #[test]
    fn run_detail_reflects_status_and_answer() {
        let ledger = Ledger::in_memory().unwrap();
        let task = Task::new("detail me");
        ledger.begin_run(&task).unwrap();
        // Running: no answer, no end time yet.
        let d = ledger.run_detail(task.id).unwrap();
        assert_eq!(d.goal, "detail me");
        assert_eq!(d.status.as_deref(), Some("running"));
        assert!(d.ended_ms.is_none());

        ledger
            .finalize(
                task.id,
                &Outcome::Finished {
                    answer: "42".into(),
                },
            )
            .unwrap();
        let d = ledger.run_detail(task.id).unwrap();
        assert_eq!(d.status.as_deref(), Some("finished"));
        assert_eq!(d.answer.as_deref(), Some("42"));
        assert!(d.ended_ms.is_some());

        assert!(matches!(
            ledger.run_detail(TaskId::new()).unwrap_err(),
            LedgerError::NotFound(_)
        ));
    }
}
