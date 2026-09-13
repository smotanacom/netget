# NetGet in the browser

The landing page (`docs/index.html`, served at netget.net) runs NetGet itself: the dashboard,
the protocol servers and the LLM plumbing, compiled to `wasm32-unknown-unknown`. This
directory holds the build script and the headless test; the code is in `crates/`.

## What runs where

| Piece | Native | Browser |
|---|---|---|
| Executor (`tokio::spawn`, timers) | tokio runtime | the JS event loop — `crates/netget-tokio-wasm` |
| Sockets (`tokio::net`) | the kernel | a virtual loopback in the same crate: `bind` claims a port in a table, `connect` hands the listener an in-memory duplex |
| Terminal | crossterm on a tty | xterm.js; `crates/netget-web/src/backend.rs` emits ANSI, keys arrive as DOM `KeyboardEvent`s, `crates/netget-crossterm-wasm` supplies crossterm's *types* |
| LLM | Ollama / OpenAI over HTTP | `LlmBackend::Bridge` (`src/llm/bridge.rs`): every request goes to the page as JSON; the page answers with WebLLM, a local Ollama, or the visitor typing |
| Clocks | std | `performance.now()` / `Date.now()` via `crate::utils::clock` |

The protocol servers (`src/server/tcp`, `telnet`, `http`) are compiled **unchanged**. On
wasm32 the names `tokio` and `crossterm` resolve to the shim crates
(`extern crate … as` in `src/lib.rs`), which is what keeps the `#[cfg]` count in protocol
code at zero. Everything platform-bound that the servers do not need — the rolling TUI,
process spawning for scripts, the HTTP client, termbg, socket2, ollama-rs — is gated with
`#[cfg(not(target_arch = "wasm32"))]`.

## Build

```bash
rustup target add wasm32-unknown-unknown
rustup component add llvm-tools            # llvm-ar, for ring's C objects (see below)
cargo install wasm-bindgen-cli --version "$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p')"
./web/build.sh                              # -> docs/demo/pkg/ (gitignored)
node web/test/smoke.mjs                     # headless end-to-end check of the bundle
cd docs && python3 -m http.server 8000      # then open http://localhost:8000/
```

`web/build.sh --dev` skips optimisation (seconds instead of a minute, ~5x the size).

Two things the script handles that are easy to lose an hour to:

- **`wasm-bindgen` must match `Cargo.lock` exactly**; the script refuses to run otherwise.
- **macOS `ar` writes a broken archive for wasm objects** ("not a mach-o file" from
  `ranlib`), which surfaces at *link* time as `undefined symbol: ring_core_0_17_14__…`. ring
  is in the build because the `http` feature pulls rustls/rcgen (unused in the browser but
  compiled). The script points cargo at `llvm-ar`; if ring was already built with the wrong
  archiver, `cargo clean -p ring --target wasm32-unknown-unknown --release` once.

## The page's contract (`crates/netget-web`)

`new NetGet({cols, rows, onOutput, onLlm, model, theme})` boots everything. Then:

- `key(json)`, `mouse(json)`, `text(str)`, `resize(cols, rows)` — terminal input.
- `set_llm_handler(fn)` — `fn(requestJson) -> Promise<replyJson | object>`. A request is
  `{id, kind: "generate" | "chat", model, messages: [{role, content}], tools: [...]}`. A
  reply is `{content?, tool_calls?: [{name, arguments}], prompt_tokens?,
  completion_tokens?}` or `{error}`.
- `start_server(json, cb)` — `cli::management::ServerForm`, the same path the dashboard's
  own form and MCP use.
- `connect(port, onData, onClose) -> id`, `send(id, bytes)`, `close(id)` — a client on the
  virtual network. `listening_ports()` and `servers(cb)` describe what is there.

`docs/js/demo.js` is the page: the dashboard terminal, a Telnet terminal (with the IAC
negotiation a plain client does), a browser that speaks HTTP/1.1 over `connect()`, a raw
socket, and the model panel with its three modes.

## Adding a protocol to the browser build

1. Enable its feature in `crates/netget-web/Cargo.toml`.
2. `cargo check -p netget-web --target wasm32-unknown-unknown`. Anything TCP-only that
   uses only `tokio::net::TcpListener`/`TcpStream`, `tokio::io`, `tokio::sync` and
   `tokio::time` compiles as is. UDP has no virtual counterpart yet
   (`UdpSocket::bind` reports `Unsupported`), and anything that shells out or opens files
   compiles but fails at runtime.
3. Replace `std::time::Instant` / `SystemTime` with `crate::utils::clock::*` in the
   protocol's source: those **panic at runtime** on wasm and the compiler cannot tell you.
4. Build, then extend `web/test/smoke.mjs` or the page with a client for it.
