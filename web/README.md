# NetGet in the browser

The landing page (`site/index.html`, served at netget.net) runs NetGet itself: the dashboard,
the protocol servers and the LLM plumbing, compiled to `wasm32-unknown-unknown`. This
directory holds the build script and the headless test; the code is in `crates/`.

## What runs where

| Piece | Native | Browser |
|---|---|---|
| Executor (`tokio::spawn`, timers) | tokio runtime | the JS event loop — `crates/netget-tokio-wasm` |
| Sockets (`tokio::net`) | the kernel | a virtual loopback in the same crate: TCP `bind` claims a port in a table and `connect` hands the listener an in-memory duplex; UDP `bind` claims a port and `send_to` delivers a datagram to the socket bound on that port |
| Terminal | crossterm on a tty | xterm.js; `crates/netget-web/src/backend.rs` emits ANSI, keys arrive as DOM `KeyboardEvent`s, `crates/netget-crossterm-wasm` supplies crossterm's *types* |
| LLM | Ollama / OpenAI over HTTP | `LlmBackend::Bridge` (`src/llm/bridge.rs`): every request goes to the page as JSON; the page answers with WebLLM, a local Ollama, or the visitor typing |
| Clocks | std | `performance.now()` / `Date.now()` via `crate::utils::clock` |

The protocol servers are compiled **unchanged**. On
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
./web/build.sh                              # -> site/demo/pkg/ (gitignored)
node web/test/smoke.mjs                     # headless end-to-end check of the bundle
cd site && python3 -m http.server 8000      # then open http://localhost:8000/
./site/deploy.sh                            # publish: S3 + CloudFront, see site/CLAUDE.md
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
- `connect(port, onData, onClose) -> id`, `send(id, bytes)`, `close(id)` — a TCP client on
  the virtual network; `udp_open(onDatagram) -> id`, `udp_send(id, port, bytes)`,
  `udp_close(id)` — a UDP one. `listening_ports()`, `bound_udp_ports()` and `servers(cb)`
  describe what is there.

`site/js/demo.js` is the page: the dashboard terminal, a Telnet terminal (with the IAC
negotiation a plain client does), a browser that speaks HTTP/1.1 over `connect()`, a raw
socket, and the model panel with its three modes.

## Which protocols are in the browser build

Everything `crates/netget-web/Cargo.toml` lists: 68 features, every server protocol that
compiles for wasm32 — TCP ones (telnet, http, http2, ftp, pop3, nntp, smb, vnc, rdp,
modbus, kafka, nats, stomp, memcached, whois, gopher, finger, ident, svn, mercurial, sip,
rtsp, hls, snowflake, db2, mongodb-server, saml, openid, oci-registry, npm, pypi, maven,
elasticsearch, jsonrpc, oauth2, openapi, openidconnect, bitcoin, torrent-*, tls, …) and
UDP ones (udp, dhcp, dhcpv6, bootp, tftp, snmp, syslog, coap, radius, ssdp, netbios-ns,
rtp, gtp, hsrp, wol, dc, …). Twelve of those have a *client* that is reqwest end to end;
the client is gated with `not(target_arch = "wasm32")` in `src/client/mod.rs` and the
registry, the server is in.

Left out, and why (re-derive with the probe below rather than trusting this):

- **A dependency needs real sockets** (`mio` on the wasm target): dns/dot/doh/llmnr
  (hickory), ntp (rsntp), ssh/ssh-agent (russh), irc, xmpp, smtp, mysql, mongodb (client
  crate), redis, postgresql, cassandra, nfs, amqp, mqtt, stun/turn/webrtc/websocket,
  socks5, mcp, grpc/etcd (tonic), zookeeper, dynamo, quic/http3 (quinn), couchdb/openai
  (tokio `net` feature).
- **A C library**: mssql, imap, ldap, proxy/http_proxy (openssl), git (the libgit2
  binding), mdns (if-addrs).
- **A crate that assumes an OS HTTP client**: ipp, xmlrpc, webdav.
- **Raw sockets, devices, or the OS itself**: arp, icmp, ospf, datalink, isis, the link
  protocols, usb-*, bluetooth-*, nfc, pty/stdio/named_pipe/socket_file, tuntap, wireguard,
  tor, openvpn. m3ua reaches for socket2 directly.

**Compiling is not running, and the hyper servers are the standing example.** `http`,
`http2`, `openapi`, `jsonrpc`, `oauth2`, `openid`, `ollama`, `elasticsearch`, `npm`, `pypi`,
`maven`, `yarn`, `spark`, `snowflake`, `rss`, `mercurial`, `oci-registry`, `saml-idp`,
`saml-sp` and `kubernetes-server` are all in the build and all bind, accept and log happily —
and every one of them kills the page on the first byte of a request. `hyper`'s HTTP/1
dispatcher calls `T::update_date()` at the top of its **first poll**, which reaches
`std::time::SystemTime::now()`; on `wasm32-unknown-unknown` that panics, and a panic inside
hyper's own date-header cache is not somewhere `crate::utils::clock` can reach. Measured
22 September 2026 against the real bundle. There is no fix short of patching hyper, so treat
"in the feature list" as "compiles", never as "works", and prove a protocol with a round trip
through `web/test/smoke.mjs` before claiming it runs.

To re-derive the list, run the probe: for each feature, `cargo check --target
wasm32-unknown-unknown --no-default-features --features tcp,udp,telnet,http,<f> --lib`
and sort the failures by where the first error lands — inside `src/client/<f>/` means the
client can be gated and the server kept; anywhere else means the feature stays out until
its dependency does.

## Adding a protocol to the browser build

1. Add its feature in `crates/netget-web/Cargo.toml`.
2. `cargo check -p netget-web --target wasm32-unknown-unknown`. Anything that uses only
   `tokio::net::{TcpListener, TcpStream, UdpSocket}`, `tokio::io`, `tokio::sync` and
   `tokio::time` compiles as is. Multicast joins and socket options are accepted and
   ignored by the virtual network. Anything that shells out or opens files compiles but
   fails at runtime.
3. `std::time::Instant` and `SystemTime` are already `crate::utils::clock::*` throughout
   `src/`; keep new code on that alias. On wasm the shim's `Instant` is its own type, so a
   stray `std::time::Instant` is a **compile** error there, not a runtime panic. A
   *dependency* calling `SystemTime::now()` is the one this does not cover — that is a
   runtime panic and it is what rules out every hyper server above.
4. The virtual `TcpStream` supports `peek`, so the first-byte deadline the hyper-based
   servers take before `serve_connection` compiles and behaves the same way here: it reads,
   holds what it read in a pushback buffer, and the next read returns those bytes first. See
   `crates/netget-tokio-wasm/tests/tcp_peek_test.rs`.
5. Build, then extend `web/test/smoke.mjs` or the page with a client for it.
