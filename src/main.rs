//! `aegis` — the Aegis command-line interface.
//!
//! Wires the crates into a usable tool:
//!   * `run`     — drive a ReAct agent under hard budgets, journaling every step
//!   * `compact` — AST-aware token compaction of a source file
//!   * `verify`  — compile a Rust project and surface structured diagnostics
//!   * `replay`  — reconstruct a past run from the SQLite ledger
//!   * `ledger`  — inspect the journal

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;

use sturdy_compact::{Compactor, Language};
use sturdy_core::{
    Action, Budget, Decision, HarnessError, Observation, Outcome, ReActEngine, Reasoner, Task,
    TaskId, Thought, ToolCall, ToolExecutor, Trajectory,
};
use sturdy_exec::{verify, CommandSpec};
use sturdy_ledger::Ledger;
use sturdy_llm::{ChatReasoner, ToolSpec};
use sturdy_mcp::McpClient;

/// A deterministic AI agent execution & verification harness.
#[derive(Parser)]
#[command(name = "aegis", version, about, long_about = None)]
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
    /// Regression-test a candidate run against a baseline suite.
    Eval(EvalArgs),
}

#[derive(Parser)]
struct EvalArgs {
    /// Path to the eval suite JSON (names the baseline ledger + metrics).
    #[arg(long)]
    suite: PathBuf,
    /// Candidate ledger (a fresh `aegis run`) to compare against the baseline.
    #[arg(long)]
    candidate: PathBuf,
    /// Report format for CI.
    #[arg(long, default_value = "pretty")]
    output_format: String,
}

// For `run`, flags override `aegis.toml`, which overrides the built-in defaults.
// Overridable settings are `Option` so we can tell "not set" from a default.
#[derive(Parser)]
struct RunArgs {
    /// The natural-language goal for the agent.
    goal: String,
    /// Config file to load (defaults to ./aegis.toml if present).
    #[arg(long)]
    config: Option<PathBuf>,
    /// SQLite ledger path. [config: db, default: aegis.sqlite]
    #[arg(long)]
    db: Option<PathBuf>,
    /// Working directory the tools operate in.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Max cumulative tokens. [config: max_tokens, default: 100000]
    #[arg(long)]
    max_tokens: Option<u64>,
    /// Max ReAct steps. [config: max_steps, default: 12]
    #[arg(long)]
    max_steps: Option<u64>,
    /// Wall-clock budget in seconds. [config: max_secs, default: 120]
    #[arg(long)]
    max_secs: Option<u64>,
    /// LLM model to drive the agent. If omitted, an offline demo policy is used.
    #[arg(long)]
    model: Option<String>,
    /// OpenAI-compatible API base URL. [config: api_base, default: local Ollama]
    #[arg(long)]
    api_base: Option<String>,
    /// Read the API key from this environment variable (e.g. OPENAI_API_KEY).
    #[arg(long)]
    api_key_env: Option<String>,
    /// Launch an MCP server as the tool source, e.g.
    /// --mcp "npx -y @modelcontextprotocol/server-filesystem .".
    #[arg(long)]
    mcp: Option<String>,
    /// Emit the result as JSON instead of the human-readable trace.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct CompactArgs {
    /// Source file to compact (language detected from the extension).
    file: PathBuf,
    /// Only compact if the file exceeds this many estimated tokens.
    #[arg(long)]
    max_tokens: Option<usize>,
    /// Force a language instead of detecting it (rust|python|javascript|typescript|go).
    #[arg(long)]
    lang: Option<String>,
    /// Emit the result as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct VerifyArgs {
    /// Project directory containing a Cargo.toml.
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Compile timeout in seconds.
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
    /// Emit the report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct ReplayArgs {
    /// The task id (UUID) to replay.
    task_id: String,
    #[arg(long, default_value = "aegis.sqlite")]
    db: PathBuf,
    /// Emit the trajectory as JSON.
    #[arg(long)]
    json: bool,
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
        #[arg(long, default_value = "aegis.sqlite")]
        db: PathBuf,
        /// Emit the listing as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one run's metadata, stats, and full trajectory.
    Show {
        /// The task id (UUID).
        task_id: String,
        #[arg(long, default_value = "aegis.sqlite")]
        db: PathBuf,
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Defaults for `run`, loaded from `aegis.toml`. Every field is optional; CLI
/// flags win, then the config, then the built-in defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    db: Option<PathBuf>,
    max_tokens: Option<u64>,
    max_steps: Option<u64>,
    max_secs: Option<u64>,
    model: Option<String>,
    api_base: Option<String>,
    api_key_env: Option<String>,
    mcp: Option<String>,
}

impl Config {
    /// Load from `path` (error if given but missing), else `./aegis.toml` if it
    /// exists, else an empty config.
    fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => {
                let default = PathBuf::from("aegis.toml");
                if !default.exists() {
                    return Ok(Config::default());
                }
                default
            }
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }
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
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
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

/// The schema advertised to the model for the built-in `shell` tool.
fn shell_tool_spec() -> ToolSpec {
    ToolSpec::new(
        "shell",
        "Run a program in the workspace and capture its output.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "the program to run" },
                "args": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["cmd"]
        }),
    )
}

/// Restore the default SIGPIPE handler so piping into `head`/`less` exits quietly
/// instead of panicking on "Broken pipe" (Rust ignores SIGPIPE by default).
#[cfg(unix)]
fn reset_sigpipe() {
    use nix::sys::signal::{signal, SigHandler, Signal};
    // Safe: installing the default disposition for SIGPIPE at process start.
    unsafe {
        let _ = signal(Signal::SIGPIPE, SigHandler::SigDfl);
    }
}
#[cfg(not(unix))]
fn reset_sigpipe() {}

#[tokio::main]
async fn main() -> Result<()> {
    reset_sigpipe();
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
        Command::Eval(a) => cmd_eval(a),
    }
}

/// Compare a candidate run against a baseline suite; exit non-zero on regression.
fn cmd_eval(a: EvalArgs) -> Result<()> {
    let format: aegis_eval::OutputFormat = a
        .output_format
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    let code =
        aegis_eval::run_eval(&a.suite, &a.candidate, format).map_err(|e| anyhow::anyhow!("{e}"))?;
    std::process::exit(code);
}

async fn cmd_run(a: RunArgs) -> Result<()> {
    // Precedence: CLI flag → config file → built-in default.
    let cfg = Config::load(a.config.as_deref())?;
    let json = a.json;
    let db =
        a.db.or(cfg.db)
            .unwrap_or_else(|| PathBuf::from("aegis.sqlite"));
    let max_tokens = a.max_tokens.or(cfg.max_tokens).unwrap_or(100_000);
    let max_steps = a.max_steps.or(cfg.max_steps).unwrap_or(12);
    let max_secs = a.max_secs.or(cfg.max_secs).unwrap_or(120);
    let api_base = a
        .api_base
        .or(cfg.api_base)
        .unwrap_or_else(|| "http://localhost:11434/v1".into());
    let model = a.model.or(cfg.model);
    let api_key_env = a.api_key_env.or(cfg.api_key_env);
    let mcp = a.mcp.or(cfg.mcp);

    let budget = Budget {
        max_tokens,
        max_steps,
        wall_clock: Duration::from_secs(max_secs),
    }
    .tracker();

    let ledger = Ledger::open(&db).context("opening ledger")?;
    let task = Task::new(a.goal.clone()).in_workspace(a.cwd.display().to_string());
    ledger.begin_run(&task)?;

    // Tool source: an MCP server if requested, otherwise the built-in shell tool.
    let (tool_specs, tools): (Vec<ToolSpec>, Arc<dyn ToolExecutor>) = match &mcp {
        Some(cmd) => {
            let parts: Vec<String> = cmd.split_whitespace().map(String::from).collect();
            let program = parts
                .first()
                .cloned()
                .context("--mcp needs a command to launch")?;
            let args: Vec<&str> = parts[1..].iter().map(String::as_str).collect();
            let client = McpClient::connect_stdio(&program, &args)
                .await
                .context("launching MCP server")?;
            let info = client.initialize("aegis").await.context("MCP initialize")?;
            let mcp_tools = client.list_tools().await.context("MCP tools/list")?;
            if !json {
                println!(
                    "  mcp: {} v{} · {} tool(s)",
                    info.name,
                    info.version,
                    mcp_tools.len()
                );
            }
            let specs = mcp_tools
                .iter()
                .map(|t| {
                    ToolSpec::new(
                        t.name.clone(),
                        t.description.clone().unwrap_or_default(),
                        t.input_schema.clone(),
                    )
                })
                .collect();
            (specs, Arc::new(client))
        }
        None => (
            vec![shell_tool_spec()],
            Arc::new(ShellTool {
                cwd: a.cwd.clone(),
                timeout: Duration::from_secs(30),
            }),
        ),
    };

    // Reasoner: a real model if configured, else the offline demo policy.
    let reasoner: Arc<dyn Reasoner> = match &model {
        Some(m) => {
            let key = api_key_env.as_ref().and_then(|e| std::env::var(e).ok());
            if !json {
                println!("  model: {m} @ {api_base}");
            }
            Arc::new(ChatReasoner::new(
                api_base.clone(),
                m.clone(),
                key,
                tool_specs,
            ))
        }
        None => Arc::new(DemoReasoner),
    };

    let engine = ReActEngine::new(reasoner, tools, budget.clone()).with_observer(ledger.observer());
    if !json {
        println!("▶ run {}\n  goal: {}\n", task.id, task.goal);
    }

    // Interactive spinner (human mode + a tty only).
    let spinner = (!json && std::io::stderr().is_terminal()).then(|| {
        let pb = ProgressBar::new_spinner();
        pb.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
        pb.set_message("running agent…");
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    });

    // Run, but allow Ctrl-C to interrupt gracefully — completed steps are already
    // journaled, so we finalize the run and reconstruct the partial trajectory.
    let (outcome, trajectory) = tokio::select! {
        result = engine.run(&task) => result,
        _ = tokio::signal::ctrl_c() => {
            let outcome = Outcome::Interrupted { reason: "interrupted by user (Ctrl-C)".into() };
            let trajectory = ledger.replay(task.id).unwrap_or_else(|_| Trajectory::new(task.id));
            (outcome, trajectory)
        }
    };
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    ledger.finalize(task.id, &outcome)?;

    if json {
        let out = serde_json::json!({
            "task_id": task.id.to_string(),
            "outcome": outcome,
            "steps": trajectory.len(),
            "tokens_used": budget.tokens_used(),
            "trajectory": trajectory,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    print_trajectory(&trajectory);
    println!();
    print_outcome(&outcome);
    println!(
        "  {} steps · {} tokens",
        trajectory.len(),
        budget.tokens_used()
    );
    println!(
        "  replay with: aegis replay {} --db {}",
        task.id,
        db.display()
    );
    Ok(())
}

fn print_outcome(o: &Outcome) {
    match o {
        Outcome::Finished { answer } => println!("✔ finished: {answer}"),
        Outcome::BudgetExhausted { reason } => println!("◼ stopped on budget: {reason}"),
        Outcome::Failed { reason } => println!("✘ failed: {reason}"),
        Outcome::Interrupted { reason } => println!("⏹ {reason}"),
    }
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
    let mut compactor = match &a.lang {
        Some(l) => Compactor::new(parse_lang(l)?)?,
        None => Compactor::for_path(&a.file)?,
    };
    let lang = compactor.language().name();
    let result = match a.max_tokens {
        Some(max) => compactor.compact_to_budget(&source, max)?,
        None => compactor.outline(&source)?,
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "{} · tokens: {} → {} ({:.0}% saved) · {} bodies elided",
        lang,
        result.original_tokens,
        result.compacted_tokens,
        result.savings() * 100.0,
        result.elided_bodies
    );
    println!("────────────────────────────────────────");
    print!("{}", result.text);
    Ok(())
}

fn parse_lang(s: &str) -> Result<Language> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Language::Rust,
        "python" | "py" => Language::Python,
        "javascript" | "js" => Language::JavaScript,
        "typescript" | "ts" => Language::TypeScript,
        "go" => Language::Go,
        other => anyhow::bail!("unknown language `{other}` (rust|python|javascript|typescript|go)"),
    })
}

async fn cmd_verify(a: VerifyArgs) -> Result<()> {
    let spinner = (!a.json && std::io::stderr().is_terminal()).then(|| {
        let pb = ProgressBar::new_spinner();
        pb.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
        pb.set_message(format!("verifying {}…", a.dir.display()));
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    });
    let report = verify(&a.dir, Duration::from_secs(a.timeout_secs)).await?;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }

    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        if !report.ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    if report.timed_out {
        println!("⏱ verification timed out");
    }
    println!(
        "{} · {} · {} error(s), {} warning(s)",
        if report.ok { "✔ ok" } else { "✘ failed" },
        report.system,
        report.errors,
        report.warnings
    );
    if let Some(reason) = &report.failure {
        // A cargo-level failure with no compiler diagnostics (e.g. no Cargo.toml).
        println!("  {}", truncate(reason, 200));
    }
    for d in report
        .diagnostics
        .iter()
        .filter(|d| d.level == "error")
        .take(10)
    {
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
    if a.json {
        println!("{}", serde_json::to_string_pretty(&trajectory)?);
        return Ok(());
    }
    println!("↺ replay {} · {} steps\n", a.task_id, trajectory.len());
    print_trajectory(&trajectory);
    println!("\n  total tokens: {}", trajectory.total_tokens());
    Ok(())
}

fn cmd_ledger(a: LedgerArgs) -> Result<()> {
    match a.command {
        LedgerCommand::List { db, json } => {
            let ledger = Ledger::open(&db).context("opening ledger")?;
            let runs = ledger.list_runs()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
                return Ok(());
            }
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
        LedgerCommand::Show { task_id, db, json } => {
            let uuid = uuid_from_str(&task_id)?;
            let ledger = Ledger::open(&db).context("opening ledger")?;
            let detail = ledger.run_detail(TaskId(uuid))?;
            let trajectory = ledger.replay(TaskId(uuid))?;

            if json {
                let out = serde_json::json!({
                    "run": detail,
                    "steps": trajectory.len(),
                    "total_tokens": trajectory.total_tokens(),
                    "trajectory": trajectory,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }

            let duration = detail
                .ended_ms
                .map(|e| format!("{}ms", (e - detail.started_ms).max(0)))
                .unwrap_or_else(|| "—".into());
            println!("run  {}", detail.task_id);
            println!("  goal:     {}", detail.goal);
            println!("  status:   {}", detail.status.as_deref().unwrap_or("?"));
            println!(
                "  steps:    {} · {} tokens · {}",
                trajectory.len(),
                trajectory.total_tokens(),
                duration
            );
            if let Some(a) = &detail.answer {
                println!("  answer:   {}", truncate(a, 200));
            }
            println!();
            print_trajectory(&trajectory);
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
