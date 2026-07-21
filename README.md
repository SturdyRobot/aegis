# SturdyHarness

[![CI](https://github.com/SturdyRobot/sturdy-harness/actions/workflows/ci.yml/badge.svg)](https://github.com/SturdyRobot/sturdy-harness/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A deterministic AI-agent execution and verification harness, written in Rust.

SturdyHarness sits between an LLM (frontier API or local Ollama) and a real
codebase. It drives a **ReAct** agent loop under **hard, enforceable budgets**,
manages the context window with **AST-aware compaction**, runs tools over the
**Model Context Protocol**, executes verification in **isolated subprocesses**,
and journals every step to **SQLite** for deterministic replay.

The design goal is *control*: an agent should never run away — not on tokens,
not on steps, not on wall-clock, and not on a build that forks a runaway child
process. Every one of those is a hard ceiling here.

```
$ sturdy run "assess the toolchain"
▶ run 442a9c30-2f2a-4dcf-9a19-524360ebc512
  [0] 🧠 Establish the toolchain before touching the project.
      → shell {"cmd":"cargo","args":["--version"]}
      ← cargo 1.95.0
  [1] 🧠 Confirm the compiler is present too.
      → shell {"cmd":"rustc","args":["--version"]}
      ← rustc 1.95.0
  [2] ⏹ finish: Toolchain verified.
✔ finished · 3 steps · 54 tokens · 35ms
  replay with: sturdy replay 442a9c30-… 
```

## Install

**Prerequisites:** a recent stable [Rust toolchain](https://rustup.rs) and a C
compiler (`cc`/`clang` on macOS/Linux, MSVC on Windows) — the Tree-sitter
grammar and bundled SQLite build a little native code. No network is required at
runtime.

Install the `sturdy` binary straight from GitHub:

```sh
cargo install --git https://github.com/SturdyRobot/sturdy-harness
```

Or clone and build from source:

```sh
git clone https://github.com/SturdyRobot/sturdy-harness
cd sturdy-harness
cargo install --path .        # installs `sturdy` into ~/.cargo/bin
# or just: cargo build --release   → target/release/sturdy
```

Then:

```sh
sturdy --help
sturdy run "assess the toolchain"
```

## Architecture

A Cargo workspace with strict separation of concerns. `sturdy-core` is the
dependency root (pure, no I/O); every satellite crate depends on it and converts
its own errors into the core taxonomy at the boundary.

| Crate | Responsibility |
|-------|----------------|
| **sturdy-core** | Domain model, the ReAct engine + validated state machine, hard budget enforcement (atomic + wall-clock), the shared error type. Pure and heavily tested. |
| **sturdy-compact** | Tree-sitter parser + token compactor. Keeps code *skeletons* (signatures, doc comments) and elides function bodies to fit a context budget. |
| **sturdy-mcp** | A native async **JSON-RPC 2.0** client for MCP over newline-delimited stdio, with concurrent request de-multiplexing. Speaks `initialize` / `tools/list` / `tools/call`. |
| **sturdy-exec** | Tokio subprocess runner. Each child leads its own **process group**, so a timeout reaps the whole subtree (`killpg`). Includes a `cargo` **diagnostic interceptor**. |
| **sturdy-ledger** | Append-only **SQLite** journal. Records each step live via the engine's observer hook and reconstructs any run byte-for-byte (`replay`). |
| **sturdy** (bin) | `clap` CLI wiring it all together. |

### The ReAct engine

The heart is a strict `Think → Act → Observe` cycle. An explicit `StateMachine`
rejects any transition outside the cycle, and the shared `BudgetTracker` is
**charged before any expensive work is done** — so exhaustion is detected
deterministically, and the engine always returns a full trajectory even when it
stops early.

Plugging in a real model is just implementing one trait:

```rust
#[async_trait]
trait Reasoner {
    async fn next_action(&self, task: &Task, trajectory: &Trajectory) -> Result<Decision>;
}
```

The bundled CLI ships a deterministic demo `Reasoner` so the whole pipeline
(budgets → state machine → tools → ledger → replay) runs offline. Swap it for an
Ollama/API client and the rest of the harness is unchanged.

## CLI

```
sturdy run <goal>          Drive an agent under budgets, journaling every step
        --max-tokens N       token ceiling (default 100k)
        --max-steps N        step ceiling (default 12)
        --max-secs N         wall-clock ceiling (default 120)
        --db <path>          SQLite ledger (default sturdy.sqlite)

sturdy compact <file>      AST-aware token compaction of a Rust file
        --max-tokens N       only compact if the file exceeds N tokens

sturdy verify [dir]        Compile a Rust project, report structured diagnostics
sturdy replay <task-id>    Reconstruct a past run from the ledger
sturdy ledger list         List every recorded run
```

## Build & test

```sh
cargo build           # workspace + `sturdy` binary
cargo test --workspace # 20 tests, all green
```

Requires a Rust toolchain and a C compiler (Tree-sitter grammars and bundled
SQLite build native code). No network is needed for the demo.

## Status

This is a working foundation with real, tested implementations of every
subsystem. The reasoner is currently a deterministic demo policy; the model
integration, an MCP-tool bridge into the engine, and a richer TUI are the next
layers to build on top of it.

## License

MIT
