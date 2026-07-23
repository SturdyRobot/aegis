# Aegis — Backlog & "To Do Later"

Ranked, honest triage of proposed features. Verdicts weigh **effort**, **real
value** (not marketing), and — because this repo doubles as a portfolio —
**whether it can actually be built and verified**, not just claimed.

Legend: 🚀 build soon · 🔧 build, scoped · 🅿️ parked (needs special env) · 🛑 skip

---

## Deferred from the feature sprint

### 🅿️ aegis-web — in-browser WASM agent (hero demo for sturdyrobot.io)
Compile `aegis-core` to `wasm32-unknown-unknown` and expose a `WasmAegisAgent`
via `wasm-bindgen` so visitors run a live agent in the retro terminal.
- **Value:** high — a click-and-watch demo is the single best recruiter hook.
- **Why parked:** real work + only partially verifiable here. `aegis-core` pulls
  **tokio** and **uuid**, which don't cleanly target `wasm32`; needs feature-gating
  (tokio → `sync`/`macros` only, `uuid` → `js` backend), a wasm-only demo reasoner
  (no `reqwest`/process tools), and an in-memory `WasmLedger`. No `wasm-pack`
  installed locally, so the final bundle can't be proven without setup.
- **When:** its own focused pass. Add wasm target → `cargo check --target
  wasm32-unknown-unknown` → build the demo reasoner → `wasm-pack build --target
  web` → drop the glue into the Vite frontend.

### 🛑 aegis-zk — SP1 zkVM execution proofs
Prove a run obeyed compliance policy without revealing prompts/PII.
- **Verdict:** skip unless there's a real SP1 host to build and verify on.
- **Why:** no SP1 toolchain locally; `sp1-sdk` is a massive, network-bound
  dependency that would bloat and likely break CI on both legs; even a *mock*
  proof can't be run here. Shipping unverified cryptographic-proof code to a
  public repo is a **credibility risk** — an engineer who sees it was never run
  trusts the whole repo less. Only worth it once you can generate + verify a real
  proof end-to-end.

---

## Red-team triage (evaluated)

The red-team writeup is genuinely good — it does the real "bullshit check." I
agree with almost all of it. My verdicts, with the engineering nuance that
matters:

### 🚀 `aegis resume <RUN_ID>` — crash recovery
- **Agree: build, near-free.** The ledger already event-sources every step, so
  resume = read the last committed step + boot the ReAct loop forward.
- **The one real design point (theirs, and it's the crux):** *idempotency.* Never
  blindly replay the last action — if the crash happened mid tool-call (sent a
  Slack message, charged a card), replaying double-executes it. Resume must start
  **after the last fully-journaled step**, and tool actions need an idempotency
  story: mark tools pure/idempotent vs. side-effecting, and for side-effecting
  ones require `--force` or a confirmation before re-running. That's the actual
  work; the plumbing is trivial.
- **Verifiable now:** yes, fully (pure ledger + core, macOS-friendly).

### 🔧 `aegis-cache` — deterministic AST/tool caching (NOT LLM responses)
- **Strong agree, exactly as scoped.** Do **not** cache LLM responses (semantic
  cache = stale/hallucinated answers in prod — a real disaster). **Do** cache
  Tree-sitter compaction keyed by `sha256(file_contents)` in SQLite: unchanged
  file → return the cached skeleton, skip the parser.
- Fits perfectly on top of `sturdy-compact` + `sturdy-ledger` you already have.
  Low effort, deterministic, safe. **Verifiable now.**

### 🔧 `aegis-policy` — lightweight user-space guardrails (NOT OPA/Rego)
- **Strong agree.** Skip OPA/Rego (heavy WASM/C bloat, against the Rust ethos).
  Write a native matcher over `aegis-policy.toml`: `blocked_tools`,
  `regex_pii_redaction`, `max_budget_per_run`.
- **Note:** complements `aegis-probe` (kernel enforcement) — this is the
  *user-space* policy layer, portable and always-on, no privileges needed.
- Low effort, **verifiable now.**

### 🚀 `aegis-bridge` — PyO3 Python bindings
- **Agree it's high-reach — with one honest correction to its framing.** "15×
  faster" is nonsense (a 1.5s LLM call dwarfs 2ms of Rust overhead); the real win
  is **distribution**: `pip install aegis-rt` puts subagent supervision, the AST
  compactor, and the SQLite event log into every FastAPI/Python codebase without a
  rewrite. That's the reach that matters.
- **Effort reality:** medium-to-more. Good cross-platform wheels (manylinux +
  macOS + Windows via `maturin`, plus a released PyPI package) is real packaging
  work, and it adds a Python API surface to maintain. Higher effort than the
  writeup's "low." Worth it **if** Python adoption is a goal.

---

## Recommended order (most leverage, least bloat, verifiable-first)

1. **`aegis resume`** — nearly free on the existing ledger; get the idempotency
   guard right. (🚀, verifiable now)
2. **`aegis-cache`** (AST + file-hash) — small, deterministic, safe, builds on
   `sturdy-compact`. (🔧, verifiable now)
3. **`aegis-policy`** (TOML matcher) — small, portable, complements `aegis-probe`.
   (🔧, verifiable now)
4. **`aegis-bridge`** (PyO3) — highest reach, but real packaging effort; do it when
   chasing Python adoption. (🚀, verifiable with the wasm/py toolchain)
5. **`aegis-web`** (WASM demo) — the recruiter hero demo; its own focused pass.
   (🅿️)
6. **`aegis-zk`** — only with a real SP1 host to verify on. (🛑 for now)

> Note vs. the writeup's "resume + PyO3" two-punch: I'd keep **resume** at #1 but
> slot **cache** and **policy** ahead of **PyO3** — they're lower-effort AND fully
> verifiable on this machine today, whereas PyO3's value is gated on cross-platform
> wheel packaging.
