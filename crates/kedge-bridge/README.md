# kedge-rt — Python bindings for Kedge

Use Kedge's Rust engine from Python. The value is **distribution, not raw speed**
(a 1.5 s LLM call dwarfs any FFI overhead) — it drops battle-tested primitives into
any FastAPI/Python codebase without a rewrite.

This crate is **not part of the Kedge Cargo workspace** (it's a `cdylib` built into
a Python wheel with `maturin`, so it needs a Python interpreter to build). The
default `cargo build --workspace` and CI never touch it.

## Build & install

```sh
python -m venv .venv && source .venv/bin/activate
pip install maturin
# abi3 forward-compat lets the wheel build on brand-new CPython (e.g. 3.14):
PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin develop --release -m crates/kedge-bridge/Cargo.toml
```

`maturin build --release` instead produces a distributable **abi3 wheel** that
installs on any CPython ≥ 3.9.

## API (`kedge_rt`)

```python
import kedge_rt, json

kedge_rt.content_hash("…")                 # sha256 cache key

r = kedge_rt.compact(source, "rust")       # AST-aware compaction
# -> {"text", "original_tokens", "compacted_tokens", "elided_bodies", "savings"}

kedge_rt.classify_tool("delete_file")      # -> ("mutating", "high")  (fail-safe)

p = kedge_rt.Policy.from_toml(toml_text)    # blocked tools + PII redaction
p.allows_tool("shell"); p.redact(text)

rep = json.loads(kedge_rt.audit_report("kedge.sqlite", price_per_1k=2.0, runs_per_day=1000))
```

## Status

First cut exposes the **synchronous** surface (compaction, hashing, tool
classification, policy, forensic audit). Async agent execution + subagent
supervision are a follow-on (they need a Tokio runtime bridged to Python async).
Verified end-to-end on CPython 3.14 (`import kedge_rt`, all functions exercised).
