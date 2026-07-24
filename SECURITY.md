# Security Policy

Kedge executes tool calls on behalf of language models. That makes its security
posture part of the product, not an afterthought — so this file describes both
how to report a problem and what the tool actually guarantees.

## Reporting a vulnerability

Email **noeljacksonjs@gmail.com** with `[kedge security]` in the subject, or open
a [private security advisory](https://github.com/SturdyRobot/kedge/security/advisories/new).

Please **do not open a public issue** for anything exploitable.

Include what you need to make it reproducible: the version or commit, the
platform, and the smallest sequence of steps that triggers it. If it involves a
tool call being executed when it should have been intercepted, the run's task id
is especially useful — `kedge replay <id>` reconstructs the exact trajectory
from the ledger.

Expect an initial response within a few days. This is a solo-maintained project,
so please be patient; I'd rather reply properly than quickly.

## Threat model

Kedge assumes the **model is untrusted** and the **operator is trusted**. Its job
is to keep a model's chosen actions inside bounds the operator set.

**What Kedge is designed to do**

- Enforce token, step, and wall-clock budgets as ceilings, checked *before* work
  happens rather than after
- Intercept mutating tool calls in `audit` (Shadow-Guard) mode so an intended
  side effect is journaled without being executed
- Fail *safe* when classifying tools: anything not recognized as clearly
  read-only is treated as mutating
- Journal every step to SQLite so any run can be replayed and audited after the
  fact

**What Kedge does not claim**

- It is **not a sandbox.** In `live` mode the agent's `shell` tool executes
  arbitrary programs with the privileges of the process that launched it. If you
  need containment, run Kedge inside one (container, VM, or the `kedge-probe`
  eBPF supervisor on Linux) — the guards are policy, not isolation.
- Tool classification is **name-based**. It is a strong default, not a proof. A
  tool named to look read-only that mutates state will be treated as read-only.
- It does not defend against a malicious *operator*, a compromised model
  endpoint, or a hostile MCP server you deliberately connected.

## Operational notes

- **API keys are read from the environment at call time and never written to the
  ledger, logs, or trajectories.** Pass a variable name (`--api-key-env`), never
  the key itself on a command line, where it would land in shell history.
- The `mcp` server speaks JSON-RPC on **stdout**; all logging goes to stderr. Do
  not add printing to stdout in that path.
- `kedge_run` over MCP defaults to `mode="audit"`. Passing `mode="live"` gives
  the model an unguarded shell — an explicit, deliberate opt-in.
- Ledgers contain full prompts, tool arguments, and outputs. Treat
  `kedge.sqlite` (and any `KEDGE_LEDGER_PATH` you set) as sensitive.

## Supported versions

Pre-1.0. Fixes land on `main`; there are no backported security branches yet.
