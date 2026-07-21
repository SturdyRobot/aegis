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
"#;

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
    pub fn finalize(&self, task_id: TaskId, outcome: &Outcome) -> Result<()> {
        let (status, answer) = match outcome {
            Outcome::Finished { answer } => ("finished", Some(answer.clone())),
            Outcome::BudgetExhausted { reason } => ("budget_exhausted", Some(reason.clone())),
            Outcome::Failed { reason } => ("failed", Some(reason.clone())),
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

/// Lightweight run listing for the CLI.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub task_id: String,
    pub goal: String,
    pub status: Option<String>,
    pub started_ms: i64,
}

/// Adapts a [`Ledger`] to the engine's [`StepObserver`] hook. Errors are logged
/// rather than propagated (the observer contract is infallible), but a failed
/// write never corrupts the run.
pub struct LedgerObserver {
    ledger: Ledger,
}

impl StepObserver for LedgerObserver {
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
}
