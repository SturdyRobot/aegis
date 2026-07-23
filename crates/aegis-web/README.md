# aegis-web

The **hero demo**: the real Aegis ReAct engine ([`sturdy-core`](../sturdy-core)),
compiled to WebAssembly and running entirely client-side. A visitor types a goal,
clicks Run, and watches the deterministic Think → Act → Observe cycle execute in
their own tab — no server, no API key, no network.

The reasoner and tools are scripted stubs (a browser sandbox has nothing real to
call), but the **engine** driving them is the genuine article: the same
state-machine validation, trajectory, and hard budget ceilings as the CLI.

## Build

This crate is **excluded from the workspace** (it's a wasm `cdylib`), so it builds
on its own. It needs the `wasm32-unknown-unknown` target and `wasm-pack`.

> **Toolchain note (macOS + Homebrew):** if Homebrew's Rust shadows rustup on
> `PATH`, cargo shells out to a host-only `rustc` with no wasm std. Prefix builds
> with the rustup toolchain's bin dir:
> `PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`.

```bash
# from the repo root
wasm-pack build crates/aegis-web --target web --release --out-dir pkg
```

Output lands in `crates/aegis-web/pkg/` (~137 KB `.wasm` + JS glue). `pkg/` is a
build artifact — not committed; regenerate with the command above.

## Run locally

```bash
python3 -m http.server 8231 --directory crates/aegis-web
# open http://localhost:8231
```

## JS API

```js
import init, { WasmAegisAgent } from "./pkg/aegis_web.js";
await init();
const agent = new WasmAegisAgent('{"max_steps":6}');
const answer = await agent.execute("Refactor the auth module", (stepJson) => {
  const step = JSON.parse(stepJson); // {index, thought, action, observation, tokens, ...}
  render(step);
});
```

`index.html` is a complete, self-contained reference UI.
