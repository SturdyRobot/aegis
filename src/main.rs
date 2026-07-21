//! `sturdy` — the SturdyHarness command-line interface.
//!
//! Wires the crates into a usable tool:
//!   * `run`     — drive a ReAct agent under hard budgets, journaling every step
//!   * `compact` — AST-aware token compaction of a source file
//!   * `verify`  — compile a Rust project and surface structured diagnostics
//!   * `replay`  — reconstruct a past run from the SQLite ledger
//!   * `ledger`  — inspect the journal

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};

use sturdy_compact::Compactor;
use sturdy_core::{
    Action, Budget, Decision, HarnessError, Observation, Outcome, ReActEngine, Reasoner, Task,
    TaskId, Thought, ToolCall, ToolExecutor, Trajectory,
};
use sturdy_exec::{verify_rust, CommandSpec};
use sturdy_ledger::Ledger;

/// A deterministic AI agent execution & verification harness.
#[derive(Parser)]
#[command(name = "sturdy", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an agent task under hard budgets, journaling every step.
    Run(RunArgs),
    /// Compact a source file by eliding function bodies (AST-aware).
    Compact(CompactArgs),
    /// Compile a Rust project and report structured diagnostics.
    Verify(VerifyArgs),
    /// Replay a past run from the ledger.
    Replay(ReplayArgs),
    /// Inspect the ledger.
    Ledger(LedgerArgs),
}

#[derive(Parser)]
struct RunArgs {
    /// The natural-language goal for the agent.
    goal: String,
    /// SQLite ledger path.
    #[arg(long, default_value = "sturdy.sqlite")]
    db: PathBuf,
    /// Working directory the tools operate in.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Max cumulative tokens.
    #[arg(long, default_value_t = 100_000)]
    max_tokens: u64,
    /// Max ReAct steps.
    #[arg(long, default_value_t = 12)]
    max_steps: u64,
    /// Wall-clock budget in seconds.
    #[arg(long, default_value_t = 120)]
    max_secs: u64,
}

#[derive(Parser)]
struct CompactArgs {
    /// Rust source file to compact.
    file: PathBuf,
    /// Only compact if the file exceeds this many estimated tokens.
    #[arg(long)]
    max_tokens: Option<usize>,
}

#[derive(Parser)]
struct VerifyArgs {
    /// Project directory containing a Cargo.toml.
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Compile timeout in seconds.
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

#[derive(Parser)]
struct ReplayArgs {
    /// The task id (UUID) to replay.
    task_id: String,
    #[arg(long, default_value = "sturdy.sqlite")]
    db: PathBuf,
}

#[derive(Parser)]
struct LedgerArgs {
    #[command(subcommand)]
    command: LedgerCommand,
}

#[derive(Subcommand)]
enum LedgerCommand {
    /// List every recorded run.
    List {
        #[arg(long, default_value = "sturdy.sqlite")]
        db: PathBuf,
    },
}

// ── a self-contained demo reasoner + shell tool ──
//
// Without an LLM configured, `run` uses a deterministic scripted policy so the
// full pipeline (budgets → state machine → tools → ledger → replay) is
// demonstrable offline. Swapping in an Ollama/API-backed `Reasoner` is a matter
// of implementing the same trait.

struct DemoReasoner;

#[async_trait]
impl Reasoner for DemoReasoner {
    async fn next_action(&self, task: &Task, traj: &Trajectory) -> sturdy_core::Result<Decision> {
        let (thought, action, tokens) = match traj.len() {
            0 => (
                "Establish the toolchain before touching the project.",
                Action::Tool(ToolCall::new(
                    "shell",
                    serde_json::json!({ "cmd": "cargo", "args": ["--version"] }),
                )),
                24,
            ),
            1 => (
                "Confirm the compiler is present too.",
                Action::Tool(ToolCall::new(
                    "shell",
                    serde_json::json!({ "cmd": "rustc", "args": ["--version"] }),
                )),
                18,
            ),
            _ => {
                let last = traj
                    .steps
                    .last()
                    .and_then(|s| s.observation.as_ref())
                    .map(|o| o.content.clone())
                    .unwrap_or_default();
                (
                    "Toolchain verified; nothing else to do for this demo goal.",
                    Action::Finish {
                        answer: format!("Goal '{}' assessed. Toolchain: {last}", task.goal),
                    },
                    12,
                )
            }
        };
        Ok(Decision {
            thought: Thought(thought.to_string()),
            action,
            tokens,
        })
    }
}

/// Executes a small built-in toolset backed by `sturdy-exec`.
struct ShellTool {
    cwd: PathBuf,
    timeout: Duration,
}

#[async_trait]
impl ToolExecutor for ShellTool {
    async fn execute(&self, call: &ToolCall) -> sturdy_core::Result<Observation> {
        match call.name.as_str() {
            "shell" => {
                let cmd = call.arguments["cmd"]
                    .as_str()
                    .ok_or_else(|| HarnessError::tool("shell", "missing `cmd`"))?;
                let args: Vec<String> = call.arguments["args"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let spec = CommandSpec::new(cmd)
                    .args(args)
                    .cwd(&self.cwd)
                    .timeout(self.timeout);
                let out = sturdy_exec::run(&spec).await.map_err(HarnessError::from)?;
                if out.success() {
                    Ok(Observation::ok(out.stdout.trim().to_string()))
                } else if out.timed_out {
                    Ok(Observation::error("command timed out"))
                } else {
                    Ok(Observation::error(format!(
                        "exit {:?}: {}",
                        out.code,
                        out.stderr.trim()
                    )))
                }
            }
            other => Ok(Observation::error(format!("unknown tool `{other}`"))),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Run(a) => cmd_run(a).await,
        Command::Compact(a) => cmd_compact(a),
        Command::Verify(a) => cmd_verify(a).await,
        Command::Replay(a) => cmd_replay(a),
        Command::Ledger(a) => cmd_ledger(a),
    }
}

async fn cmd_run(a: RunArgs) -> Result<()> {
    let budget = Budget {
        max_tokens: a.max_tokens,
        max_steps: a.max_steps,
        wall_clock: Duration::from_secs(a.max_secs),
    }
    .tracker();

    let ledger = Ledger::open(&a.db).context("opening ledger")?;
    let task = Task::new(a.goal.clone()).in_workspace(a.cwd.display().to_string());
    ledger.begin_run(&task)?;

    let engine = ReActEngine::new(
        Arc::new(DemoReasoner),
        Arc::new(ShellTool {
            cwd: a.cwd,
            timeout: Duration::from_secs(30),
        }),
        budget.clone(),
    )
    .with_observer(ledger.observer());

    println!("▶ run {}\n  goal: {}\n", task.id, task.goal);
    let (outcome, trajectory) = engine.run(&task).await;
    ledger.finalize(task.id, &outcome)?;

    print_trajectory(&trajectory);
    println!();
    match &outcome {
        Outcome::Finished { answer } => println!("✔ finished: {answer}"),
        Outcome::BudgetExhausted { reason } => println!("◼ stopped on budget: {reason}"),
        Outcome::Failed { reason } => println!("✘ failed: {reason}"),
    }
    println!(
        "  {} steps · {} tokens · {}ms",
        trajectory.len(),
        budget.tokens_used(),
        budget.budget().wall_clock.as_millis() - budget.time_remaining().as_millis()
    );
    println!("  replay with: sturdy replay {} --db {}", task.id, a.db.display());
    Ok(())
}

fn print_trajectory(t: &Trajectory) {
    for step in &t.steps {
        println!("  [{}] 🧠 {}", step.index, step.thought.0);
        match &step.action {
            Action::Tool(c) => println!("      → {} {}", c.name, c.arguments),
            Action::Finish { answer } => println!("      ⏹ finish: {answer}"),
        }
        if let Some(obs) = &step.observation {
            let marker = if obs.is_error { "⚠" } else { "←" };
            println!("      {marker} {}", truncate(&obs.content, 200));
        }
    }
}

fn cmd_compact(a: CompactArgs) -> Result<()> {
    let source = std::fs::read_to_string(&a.file)
        .with_context(|| format!("reading {}", a.file.display()))?;
    let mut compactor = Compactor::rust()?;
    let result = match a.max_tokens {
        Some(max) => compactor.compact_to_budget(&source, max)?,
        None => compactor.outline(&source)?,
    };
    println!(
        "tokens: {} → {} ({:.0}% saved) · {} bodies elided",
        result.original_tokens,
        result.compacted_tokens,
        result.savings() * 100.0,
        result.elided_bodies
    );
    println!("────────────────────────────────────────");
    print!("{}", result.text);
    Ok(())
}

async fn cmd_verify(a: VerifyArgs) -> Result<()> {
    println!("compiling {} ...", a.dir.display());
    let report = verify_rust(&a.dir, Duration::from_secs(a.timeout_secs)).await?;
    if report.timed_out {
        println!("⏱ verification timed out");
    }
    println!(
        "{} · {} error(s), {} warning(s)",
        if report.ok { "✔ ok" } else { "✘ failed" },
        report.errors,
        report.warnings
    );
    for d in report.diagnostics.iter().filter(|d| d.level == "error").take(10) {
        match (&d.file, d.line) {
            (Some(f), Some(l)) => println!("  {}:{} — {}", f, l, truncate(&d.message, 160)),
            _ => println!("  {}", truncate(&d.message, 160)),
        }
    }
    if !report.ok {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_replay(a: ReplayArgs) -> Result<()> {
    let uuid = uuid_from_str(&a.task_id)?;
    let ledger = Ledger::open(&a.db).context("opening ledger")?;
    let trajectory = ledger.replay(TaskId(uuid))?;
    println!("↺ replay {} · {} steps\n", a.task_id, trajectory.len());
    print_trajectory(&trajectory);
    println!("\n  total tokens: {}", trajectory.total_tokens());
    Ok(())
}

fn cmd_ledger(a: LedgerArgs) -> Result<()> {
    match a.command {
        LedgerCommand::List { db } => {
            let ledger = Ledger::open(&db).context("opening ledger")?;
            let runs = ledger.list_runs()?;
            if runs.is_empty() {
                println!("(no runs recorded)");
            }
            for r in runs {
                println!(
                    "{}  {:<18}  {}",
                    r.task_id,
                    r.status.unwrap_or_else(|| "?".into()),
                    truncate(&r.goal, 60)
                );
            }
            Ok(())
        }
    }
}

fn uuid_from_str(s: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).with_context(|| format!("`{s}` is not a valid task id"))
}

fn truncate(s: &str, max: usize) -> String {
    let flat = s.replace('\n', " ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}
