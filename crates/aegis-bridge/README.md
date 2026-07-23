# aegis-rt — Python bindings for Aegis

Use Aegis's Rust engine from Python. The value is **distribution, not raw speed**
(a 1.5 s LLM call dwarfs any FFI overhead) — it drops battle-tested primitives into
any FastAPI/Python codebase without a rewrite.

This crate is **not part of the Aegis Cargo workspace** (it's a `cdylib` built into
a Python wheel with `maturin`, so it needs a Python interpreter to build). The
default `cargo build --workspace` and CI never touch it.

## Build & install

```sh
python -m venv .venv && source .venv/bin/activate
pip install maturin
# abi3 forward-compat lets the wheel build on brand-new CPython (e.g. 3.14):
PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin develop --release -m crates/aegis-bridge/Cargo.toml
```

`maturin build --release` instead produces a distributable **abi3 wheel** that
installs on any CPython ≥ 3.9.

## API (`aegis_rt`)

```python
import aegis_rt, json

aegis_rt.content_hash("…")                 # sha256 cache key

r = aegis_rt.compact(source, "rust")       # AST-aware compaction
# -> {"text", "original_tokens", "compacted_tokens", "elided_bodies", "savings"}

aegis_rt.classify_tool("delete_file")      # -> ("mutating", "high")  (fail-safe)

p = aegis_rt.Policy.from_toml(toml_text)    # blocked tools + PII redaction
p.allows_tool("shell"); p.redact(text)

rep = json.loads(aegis_rt.audit_report("aegis.sqlite", price_per_1k=2.0, runs_per_day=1000))
```

## Status

First cut exposes the **synchronous** surface (compaction, hashing, tool
classification, policy, forensic audit). Async agent execution + subagent
supervision are a follow-on (they need a Tokio runtime bridged to Python async).
Verified end-to-end on CPython 3.14 (`import aegis_rt`, all functions exercised).
