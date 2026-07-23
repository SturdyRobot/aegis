# Aegis — Backlog & Roadmap

Legend: ✅ shipped · 🚀 do next · ⏳ later · 🛑 skip (for now)

---

## ✅ Shipped (on `main`, all tests green)

The core runtime and the whole trust/ops surface are done:

- **ReAct core** — hard token/step/wall-clock budgets, validated state machine,
  byte-for-byte SQLite replay.
- **Shadow-Guard audit** (`aegis run --audit` + `aegis audit`) — zero-risk dry-run
  of mutating tools + forensic ROI/security report.
- **Human-in-the-loop** (`aegis run --hitl`) — pause + approve each mutation.
- **Crash recovery** (`aegis resume`) — continue from the last journaled step.
- **Subagent mesh** (`aegis-mesh`) — bounded, isolated, contained failures.
- **MCP client** (`aegis-mcp`) — stdio **and** streamable HTTP.
- **Regression harness** (`aegis eval`) — JUnit + CI exit codes.
- **AST compaction** + **content-hashed cache** (`aegis-cache`).
- **Guardrails** — `aegis-policy` (user-space) + `aegis-probe` (eBPF LSM, Linux).
- **OpenTelemetry** export (`--features otel`).
- **Python bindings** (`aegis-bridge` → `pip install aegis-rt`) — verified on
  CPython 3.14 (see below).

---

## Language reach — the roadmap

### ✅ 1. Python (`aegis-bridge` / maturin) — SHIPPED
`pip install aegis-rt`. Python is the king of AI — this gets Aegis into 80%+ of
real-world AI pipelines. Exposes AST compaction, content hashing, tool
classification, the policy matcher, and the forensic audit as an **abi3 wheel**
(one wheel, CPython ≥3.9). Verified end-to-end with a real `import aegis_rt`.
*Follow-on:* async agent execution + subagent supervision from Python.

### 🚀 2. WebAssembly (`aegis-web`) — DO NEXT
Powers the live in-browser terminal on **sturdyrobot.io** — the visual showpiece
for recruiters, investors, and engineers. Needs `wasm-pack` + making `aegis-core`
wasm-clean (feature-gate tokio/uuid, a wasm reasoner, an in-memory ledger). The
highest-leverage item for the hiring goal specifically (click-and-watch demo).

### ⏳ 3. TypeScript (`napi-rs`) — POST-LAUNCH
`npm install aegis-rt`. TypeScript/Node.js is the second-largest AI ecosystem
(Vercel AI SDK, LangChain.js). Build **after** the Python release is stable and
only if demand shows up.

### 🛑 4. Everyone else (Go, Java, C#, …) — DON'T build native bindings
Aegis already speaks **MCP over stdio and HTTP**. A Go/Java/C# team runs the
`aegis` binary as a background process and sends JSON-RPC/MCP requests — **$0
additional code**. The daemon covers every other language for free.

> **Golden rule:** only write a custom native binding when demand forces it. The
> day a Fortune 500 shows up with a signed contract that says "we buy Aegis today
> if you ship a native Java SDK" is the day you build the Java binding — not
> before. Python + WASM + the CLI/MCP daemon covers ~99% of use cases.

---

## Other parked

### 🛑 aegis-zk — SP1 zkVM execution proofs
Zero-knowledge proof of policy-compliant execution. Skip until there's a real SP1
host to *generate and verify* a proof on — `sp1-sdk` is a massive dep and can't be
validated in the current environment. Shipping unverified crypto code is a
credibility risk.

---

## Recommended order
1. **WASM demo** (`aegis-web`) — the hiring showpiece. 🚀
2. **TypeScript** (`napi-rs`) — after Python is stable, on demand. ⏳
3. Everything else → the **MCP daemon**, not a binding. 🛑
4. **aegis-zk** — only with an SP1 host. 🛑
