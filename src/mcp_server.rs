//! `aegis mcp` — an MCP (Model Context Protocol) **server** over stdio.
//!
//! Aegis is normally an MCP *client* (it consumes tools); this flips it around so
//! any MCP client — Claude Code, in particular — can call Aegis's own
//! capabilities as native tools:
//!
//! * `aegis_compact` — AST-aware token compaction of a source file (deterministic)
//! * `aegis_audit`   — forensic cost/security report from a ledger (deterministic)
//! * `aegis_run`     — a bounded, journaled ReAct agent driven by a Groq model
//!
//! Transport is newline-delimited JSON-RPC 2.0 on stdin/stdout, per the MCP stdio
//! spec. **stdout carries the protocol** — so the handlers here call the crates
//! directly and *return* values; they never print. (All logging goes to stderr;
//! see `telemetry`.)

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};

use sturdy_compact::Compactor;
use sturdy_core::{Budget, ReActEngine, Reasoner, Task, ToolExecutor};
use sturdy_ledger::Ledger;
use sturdy_llm::ChatReasoner;

use crate::{parse_lang, shell_tool_spec, ShellTool};

/// MCP protocol revision we implement. We echo the client's requested version
/// when it sends one, falling back to this.
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Groq's OpenAI-compatible endpoint. `aegis_run` is wired to it; the key comes
/// from `GROQ_API_KEY` at call time and is never persisted.
const GROQ_API_BASE: &str = "https://api.groq.com/openai/v1";
const DEFAULT_GROQ_MODEL: &str = "llama-3.3-70b-versatile";

/// Serve the MCP protocol on stdio until stdin closes.
pub async fn serve_stdio() -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    eprintln!(
        "aegis mcp: ready on stdio · tools: aegis_compact, aegis_audit, aegis_run \
         (run needs GROQ_API_KEY)"
    );

    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                write_msg(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0", "id": null,
                        "error": { "code": -32700, "message": format!("parse error: {e}") }
                    }),
                )
                .await?;
                continue;
            }
        };

        // A request has an `id`; a notification does not (and gets no reply).
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION)
                    .to_string();
                let result = json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "aegis", "version": env!("CARGO_PKG_VERSION") },
                });
                reply(&mut stdout, id, Ok(result)).await?;
            }
            "ping" => reply(&mut stdout, id, Ok(json!({}))).await?,
            "tools/list" => reply(&mut stdout, id, Ok(json!({ "tools": tool_specs() }))).await?,
            "tools/call" => {
                // A tool failure is a *successful* JSON-RPC result carrying
                // `isError: true`, per MCP — not a protocol-level error.
                let result = handle_tool_call(&params).await;
                reply(&mut stdout, id, Ok(result)).await?;
            }
            // Notifications and anything we don't implement.
            "notifications/initialized" | "initialized" => {}
            other => {
                if id.is_some() {
                    reply(
                        &mut stdout,
                        id,
                        Err((-32601, format!("method not found: {other}"))),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

/// Write one newline-delimited JSON message and flush.
async fn write_msg(out: &mut Stdout, msg: &Value) -> Result<()> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    out.write_all(s.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

/// Reply to a request (no-op for notifications, which have no `id`).
async fn reply(
    out: &mut Stdout,
    id: Option<Value>,
    result: std::result::Result<Value, (i64, String)>,
) -> Result<()> {
    let Some(id) = id else { return Ok(()) };
    let msg = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    };
    write_msg(out, &msg).await
}

/// The tools advertised to the client via `tools/list`.
fn tool_specs() -> Value {
    json!([
        {
            "name": "aegis_compact",
            "description": "AST-aware token compaction. Parses a source file with Tree-sitter and returns its structural skeleton — signatures and types kept, function bodies elided — so a large file fits a token budget. Deterministic, no LLM. Pass `path` (read from disk, language auto-detected) OR `code` + `lang`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to a source file; language detected from its extension." },
                    "code": { "type": "string", "description": "Raw source text (use together with `lang` instead of `path`)." },
                    "lang": { "type": "string", "enum": ["rust", "python", "javascript", "typescript", "go"], "description": "Force or declare the language." },
                    "max_tokens": { "type": "integer", "description": "Elide only enough bodies to fit this token budget. Omit for a full outline (all bodies elided)." },
                    "db": { "type": "string", "description": "Ledger to journal the token savings into for cumulative reporting (default: aegis.sqlite)." }
                }
            }
        },
        {
            "name": "aegis_audit",
            "description": "Forensic report from an Aegis SQLite ledger: total runs, tokens consumed, intercepted mutations (Shadow-Guard dry-runs), and — when pricing is supplied — a cost projection. Deterministic, no LLM.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "db": { "type": "string", "description": "Ledger path (default: aegis.sqlite)." },
                    "price_per_1k": { "type": "number", "description": "USD per 1k tokens, to compute a cost figure." },
                    "runs_per_day": { "type": "integer", "description": "Expected runs/day, for a projection (needs price_per_1k)." }
                }
            }
        },
        {
            "name": "aegis_run",
            "description": "Run a bounded, fully-journaled ReAct agent on a natural-language goal, driven by a Groq model. Enforces hard budgets (steps / tokens / wall-clock) and records every step to a SQLite ledger you can later replay or audit. Returns the final answer plus the full trajectory. The agent gets a `shell` tool scoped to `cwd`. Requires GROQ_API_KEY in the environment. SAFETY: defaults to mode=\"audit\" (Shadow-Guard dry-run) — mutating tool calls are intercepted and journaled, never executed. Pass mode=\"live\" only when the caller genuinely intends real side effects.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "What the agent should accomplish." },
                    "mode": { "type": "string", "enum": ["audit", "deny", "live"], "description": "Safety posture. audit (DEFAULT) = Shadow-Guard dry-run: read-only tools run for real, mutating tools are intercepted and journaled but NOT executed. deny = mutating tools are refused outright. live = execute everything, no guard (explicit opt-in — the agent has an arbitrary shell)." },
                    "model": { "type": "string", "description": "Groq model id (default: llama-3.3-70b-versatile)." },
                    "cwd": { "type": "string", "description": "Working directory for the shell tool (default: '.')." },
                    "max_steps": { "type": "integer", "description": "Max ReAct steps (default: 12)." },
                    "max_tokens": { "type": "integer", "description": "Max cumulative tokens (default: 100000)." },
                    "max_secs": { "type": "integer", "description": "Wall-clock budget in seconds (default: 120)." },
                    "db": { "type": "string", "description": "Ledger path (default: aegis.sqlite)." }
                },
                "required": ["goal"]
            }
        }
    ])
}

/// Dispatch a `tools/call`, wrapping the outcome as an MCP `CallToolResult`.
async fn handle_tool_call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome: Result<String> = match name {
        "aegis_compact" => tool_compact(&args),
        "aegis_audit" => tool_audit(&args),
        "aegis_run" => tool_run(&args).await,
        other => Err(anyhow::anyhow!("unknown tool `{other}`")),
    };

    match outcome {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(e) => {
            json!({ "content": [{ "type": "text", "text": format!("error: {e:#}") }], "isError": true })
        }
    }
}

fn tool_compact(args: &Value) -> Result<String> {
    let lang = args.get("lang").and_then(Value::as_str);
    let (source, mut compactor) = if let Some(p) = args.get("path").and_then(Value::as_str) {
        let src = std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?;
        let compactor = match lang {
            Some(l) => Compactor::new(parse_lang(l)?)?,
            None => Compactor::for_path(Path::new(p))?,
        };
        (src, compactor)
    } else if let Some(code) = args.get("code").and_then(Value::as_str) {
        let l = lang.context("`lang` is required when passing `code`")?;
        (code.to_string(), Compactor::new(parse_lang(l)?)?)
    } else {
        anyhow::bail!("provide `path`, or `code` together with `lang`");
    };

    let result = match args.get("max_tokens").and_then(Value::as_u64) {
        Some(max) => compactor.compact_to_budget(&source, max as usize)?,
        None => compactor.outline(&source)?,
    };

    // Surface the savings explicitly so callers never have to do the subtraction.
    let saved = result
        .original_tokens
        .saturating_sub(result.compacted_tokens);
    let pct = result.savings() * 100.0;
    let summary = format!(
        "Saved {saved} tokens ({pct:.0}%): {} → {} tokens, {} bodies elided",
        result.original_tokens, result.compacted_tokens, result.elided_bodies
    );
    // Best-effort: journal this saving so `aegis_audit` can report a cumulative
    // "tokens saved" total. A ledger problem never fails the compaction itself.
    let db = args
        .get("db")
        .and_then(Value::as_str)
        .unwrap_or("aegis.sqlite");
    let cumulative = match Ledger::open(db) {
        Ok(ledger) => {
            let label = args.get("path").and_then(Value::as_str);
            if let Err(e) = ledger.record_compaction(
                result.original_tokens as u64,
                result.compacted_tokens as u64,
                label,
            ) {
                eprintln!("aegis mcp: compaction not journaled ({e})");
            }
            ledger.compaction_totals().ok()
        }
        Err(e) => {
            eprintln!("aegis mcp: compaction ledger unavailable ({e})");
            None
        }
    };

    let out = json!({
        "tokens_saved": saved,
        "percent_saved": (pct * 10.0).round() / 10.0,
        "original_tokens": result.original_tokens,
        "compacted_tokens": result.compacted_tokens,
        "elided_bodies": result.elided_bodies,
        "summary": summary,
        "cumulative": cumulative.map(|t| json!({
            "compactions": t.compactions,
            "tokens_saved": t.tokens_saved,
            "note": "running total in this ledger — see aegis_audit for the full report",
        })),
        "text": result.text,
    });
    Ok(serde_json::to_string_pretty(&out)?)
}

fn tool_audit(args: &Value) -> Result<String> {
    let db = args
        .get("db")
        .and_then(Value::as_str)
        .unwrap_or("aegis.sqlite");
    let price = args.get("price_per_1k").and_then(Value::as_f64);
    let runs = args.get("runs_per_day").and_then(Value::as_u64);
    let report = aegis_audit::AuditReport::from_ledger(Path::new(db), price, runs)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(report.to_json())
}

/// The safety posture `aegis_run` executes under. Parsed from the `mode`
/// argument; anything unrecognized is rejected rather than silently downgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    /// Shadow-Guard dry-run: read-only tools run for real, mutating tools are
    /// intercepted and journaled but never executed. The default.
    Audit,
    /// Read-only lockdown: mutating tools are refused outright.
    Deny,
    /// No guard — the agent gets an unrestricted shell. Explicit opt-in only.
    Live,
}

impl RunMode {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "audit" => Ok(RunMode::Audit),
            "deny" => Ok(RunMode::Deny),
            "live" => Ok(RunMode::Live),
            other => anyhow::bail!(
                "unknown mode `{other}` — expected \"audit\" (default), \"deny\", or \"live\""
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            RunMode::Audit => "audit",
            RunMode::Deny => "deny",
            RunMode::Live => "live",
        }
    }
}

/// Wrap `base` in the guard implied by `mode`. Returns the executor to hand the
/// engine, plus the `AuditExecutor` handle when one is in play (so the caller can
/// report how many mutations were intercepted).
///
/// `CliApprover` is deliberately never used here: it prints to stdout and reads
/// stdin, which are this process's JSON-RPC channel.
fn build_guarded_tools(
    mode: RunMode,
    base: Arc<dyn ToolExecutor>,
    ledger: Option<Arc<Ledger>>,
    run_id: sturdy_core::TaskId,
) -> (
    Arc<dyn ToolExecutor>,
    Option<Arc<aegis_audit::AuditExecutor>>,
) {
    match mode {
        RunMode::Live => (base, None),
        RunMode::Deny => (
            Arc::new(aegis_hitl::ApprovalGate::new(
                base,
                Arc::new(aegis_hitl::DenyingApprover),
                ledger,
                run_id,
            )),
            None,
        ),
        RunMode::Audit => {
            let ae = Arc::new(aegis_audit::AuditExecutor::new(base, ledger, run_id));
            (ae.clone() as Arc<dyn ToolExecutor>, Some(ae))
        }
    }
}

async fn tool_run(args: &Value) -> Result<String> {
    let goal = args
        .get("goal")
        .and_then(Value::as_str)
        .context("`goal` is required")?;
    // Validate arguments before touching the environment, so a bad `mode` is
    // reported as such instead of being masked by a missing API key.
    let mode = RunMode::parse(args.get("mode").and_then(Value::as_str).unwrap_or("audit"))?;
    let key = std::env::var("GROQ_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .context(
            "GROQ_API_KEY is not set in this process's environment — \
             the run tool needs it to reach Groq. Set it in the MCP server's env.",
        )?;

    let model = args
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_GROQ_MODEL)
        .to_string();
    let cwd = PathBuf::from(args.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let max_tokens = args
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(100_000);
    let max_steps = args.get("max_steps").and_then(Value::as_u64).unwrap_or(12);
    let max_secs = args.get("max_secs").and_then(Value::as_u64).unwrap_or(120);
    let db = PathBuf::from(
        args.get("db")
            .and_then(Value::as_str)
            .unwrap_or("aegis.sqlite"),
    );

    let budget = Budget {
        max_tokens,
        max_steps,
        wall_clock: Duration::from_secs(max_secs),
    }
    .tracker();

    let ledger = Ledger::open(&db).context("opening ledger")?;
    let task = Task::new(goal).in_workspace(cwd.display().to_string());
    ledger.begin_run(&task)?;

    // ── Safety layering (the caller here is an LLM, so default to safe) ──
    //
    // The agent's `shell` tool executes arbitrary programs. Over MCP there is no
    // human in the loop by default, so a bare ShellTool would hand an unguarded
    // shell to whatever asked. Modes mirror the CLI's `--audit` / `--hitl`:
    //
    //   audit (default) — Shadow-Guard dry-run: read-only tools execute for real,
    //                     every mutating tool is intercepted and journaled but
    //                     NOT executed.
    //   deny            — read-only lockdown: mutating tools are refused outright
    //                     (the agent observes the denial and can adapt).
    //   live            — no guard. Explicit opt-in.
    //
    // NOTE: `CliApprover` is deliberately unavailable here — it prints to stdout
    // and reads stdin, which are this process's JSON-RPC channel. Interactive
    // approval belongs on the `WebhookApprover` + `aegis serve` path.
    let base: Arc<dyn ToolExecutor> = Arc::new(ShellTool {
        cwd: cwd.clone(),
        timeout: Duration::from_secs(30),
    });
    let (tools, auditor) = build_guarded_tools(mode, base, Some(Arc::new(ledger.clone())), task.id);

    let reasoner: Arc<dyn Reasoner> = Arc::new(ChatReasoner::new(
        GROQ_API_BASE.to_string(),
        model.clone(),
        Some(key),
        vec![shell_tool_spec()],
    ));

    let engine = ReActEngine::new(reasoner, tools, budget.clone()).with_observer(ledger.observer());
    let (outcome, trajectory) = engine.run(&task).await;
    ledger.finalize(task.id, &outcome)?;

    // Report the safety posture alongside the result: a caller must never be able
    // to mistake an intercepted dry-run for work that actually happened.
    let intercepted = auditor.as_ref().map(|a| a.intercepted());
    let note = match (mode, intercepted) {
        (RunMode::Audit, Some(n)) if n > 0 => Some(format!(
            "shadow-audit: {n} mutating tool call(s) were INTERCEPTED and never executed. \
             Nothing was written. Re-run with mode=\"live\" to execute for real."
        )),
        (RunMode::Audit, _) => Some(
            "shadow-audit: no mutating tool calls were attempted; read-only work ran for real."
                .to_string(),
        ),
        (RunMode::Deny, _) => {
            Some("read-only lockdown: mutating tool calls were refused.".to_string())
        }
        (RunMode::Live, _) => None,
    };

    let out = json!({
        "task_id": task.id.to_string(),
        "model": model,
        "mode": mode.as_str(),
        "intercepted_mutations": intercepted,
        "note": note,
        "outcome": outcome,
        "steps": trajectory.len(),
        "tokens_used": budget.tokens_used(),
        "trajectory": trajectory,
    });
    Ok(serde_json::to_string_pretty(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use sturdy_core::{Observation, TaskId, ToolCall};

    /// Stands in for the real `ShellTool` and records whether it actually ran.
    struct SpyTool(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl ToolExecutor for SpyTool {
        async fn execute(&self, _call: &ToolCall) -> sturdy_core::Result<Observation> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Observation::ok("the underlying tool really executed"))
        }
    }

    fn spy() -> (Arc<dyn ToolExecutor>, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        (Arc::new(SpyTool(hits.clone())), hits)
    }

    fn call(name: &str) -> ToolCall {
        ToolCall::new(name, json!({ "cmd": "rm", "args": ["-rf", "/"] }))
    }

    #[test]
    fn unknown_modes_are_rejected_never_silently_downgraded() {
        assert_eq!(RunMode::parse("audit").unwrap(), RunMode::Audit);
        assert_eq!(RunMode::parse("deny").unwrap(), RunMode::Deny);
        assert_eq!(RunMode::parse("live").unwrap(), RunMode::Live);
        // A typo must be an error, not a silent fallback to something permissive.
        assert!(RunMode::parse("Live").is_err());
        assert!(RunMode::parse("").is_err());
        assert!(RunMode::parse("yolo").is_err());
    }

    #[tokio::test]
    async fn audit_mode_intercepts_mutating_tools_without_executing_them() {
        let (base, hits) = spy();
        let (tools, auditor) = build_guarded_tools(RunMode::Audit, base, None, TaskId::new());
        let _ = tools.execute(&call("shell")).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "audit mode MUST NOT execute a mutating tool"
        );
        assert_eq!(
            auditor
                .expect("audit mode exposes the auditor")
                .intercepted(),
            1
        );
    }

    #[tokio::test]
    async fn audit_mode_still_executes_read_only_tools() {
        let (base, hits) = spy();
        let (tools, _) = build_guarded_tools(RunMode::Audit, base, None, TaskId::new());
        let _ = tools.execute(&call("read_file")).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "read-only tools should still run for real in audit mode"
        );
    }

    #[tokio::test]
    async fn deny_mode_refuses_mutating_tools() {
        let (base, hits) = spy();
        let (tools, auditor) = build_guarded_tools(RunMode::Deny, base, None, TaskId::new());
        let _ = tools.execute(&call("shell")).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0, "deny mode MUST NOT execute");
        assert!(auditor.is_none());
    }

    #[tokio::test]
    async fn live_mode_executes_for_real() {
        let (base, hits) = spy();
        let (tools, auditor) = build_guarded_tools(RunMode::Live, base, None, TaskId::new());
        let _ = tools.execute(&call("shell")).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "live mode is the opt-in escape hatch"
        );
        assert!(auditor.is_none());
    }

    #[test]
    fn run_tool_schema_advertises_the_safety_modes() {
        let specs = tool_specs();
        let run = specs
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "aegis_run")
            .expect("aegis_run is advertised");
        assert_eq!(
            run["inputSchema"]["properties"]["mode"]["enum"],
            json!(["audit", "deny", "live"])
        );
        assert!(run["description"].as_str().unwrap().contains("SAFETY"));
    }

    #[test]
    fn every_advertised_tool_has_a_name_and_schema() {
        for t in tool_specs().as_array().unwrap() {
            assert!(t["name"].as_str().is_some_and(|n| !n.is_empty()));
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["description"].as_str().is_some_and(|d| d.len() > 40));
        }
    }
}
