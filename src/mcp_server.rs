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
            "description": "Run a bounded, fully-journaled ReAct agent on a natural-language goal, driven by a Groq model. Enforces hard budgets (steps / tokens / wall-clock) and records every step to a SQLite ledger you can later replay or audit. Returns the final answer plus the full trajectory. The agent gets a `shell` tool scoped to `cwd`. Requires GROQ_API_KEY in the environment.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "What the agent should accomplish." },
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

async fn tool_run(args: &Value) -> Result<String> {
    let goal = args
        .get("goal")
        .and_then(Value::as_str)
        .context("`goal` is required")?;
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

    let tools: Arc<dyn ToolExecutor> = Arc::new(ShellTool {
        cwd: cwd.clone(),
        timeout: Duration::from_secs(30),
    });
    let reasoner: Arc<dyn Reasoner> = Arc::new(ChatReasoner::new(
        GROQ_API_BASE.to_string(),
        model.clone(),
        Some(key),
        vec![shell_tool_spec()],
    ));

    let engine = ReActEngine::new(reasoner, tools, budget.clone()).with_observer(ledger.observer());
    let (outcome, trajectory) = engine.run(&task).await;
    ledger.finalize(task.id, &outcome)?;

    let out = json!({
        "task_id": task.id.to_string(),
        "model": model,
        "outcome": outcome,
        "steps": trajectory.len(),
        "tokens_used": budget.tokens_used(),
        "trajectory": trajectory,
    });
    Ok(serde_json::to_string_pretty(&out)?)
}
