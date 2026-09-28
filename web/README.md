# NetGet in the browser

The landing page (`site/index.html`, served at netget.net) runs NetGet itself: the dashboard,
the protocol servers and the LLM plumbing, compiled to `wasm32-unknown-unknown`. This
directory holds the build script and the headless tests; the code is in `crates/`.

## What runs where

| Piece | Native | Browser |
|---|---|---|
| Executor (`tokio::spawn`, timers) | tokio runtime | the JS event loop — `crates/netget-tokio-wasm` |
| Sockets (`tokio::net`) | the kernel | a virtual loopback in the same crate: TCP `bind` claims a port in a table and `connect` hands the listener an in-memory duplex; UDP `bind` claims a port and `send_to` delivers a datagram to the socket bound on that port |
| Terminal | crossterm on a tty | xterm.js; `crates/netget-web/src/backend.rs` emits ANSI, keys arrive as DOM `KeyboardEvent`s, `crates/netget-crossterm-wasm` supplies crossterm's *types* |
| LLM | Ollama / OpenAI over HTTP | `LlmBackend::Bridge` (`src/llm/bridge.rs`): every request goes to the page as JSON; the page answers with Chrome's built-in model (the Prompt API), a WebLLM model, or the visitor through a form |
| Clocks | std | `performance.now()` / `Date.now()` via `crate::utils::clock` |

The protocol servers are compiled **unchanged**. On
wasm32 the names `tokio` and `crossterm` resolve to the shim crates
(`extern crate … as` in `src/lib.rs`), which is what keeps the `#[cfg]` count in protocol
code at zero. Everything platform-bound that the servers do not need — the rolling TUI,
process spawning for scripts, reqwest, termbg, socket2, ollama-rs — is gated with
`#[cfg(not(target_arch = "wasm32"))]`. The `http` *client* is in the browser build: there it
speaks HTTP/1.1 through `src/client/http/transport.rs` (hyper's client over the virtual
loopback) instead of reqwest; see below.

## Build

```bash
rustup target add wasm32-unknown-unknown
rustup component add llvm-tools            # llvm-ar, for ring's C objects (see below)
cargo install wasm-bindgen-cli --version "$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p')"
./web/build.sh                              # -> site/demo/pkg/ (gitignored)
node web/test/smoke.mjs                     # headless end-to-end check of the bundle
python3 web/test/page_composer.py           # the real page in headless Chromium (Playwright; not in CI)
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
- `set_model(name)` — the model named in every request and in the dashboard's status bar
  (`llm: …`); `set_models(json)` sets what `/model` lists and should include it.
- `set_llm_handler(fn)` — `fn(requestJson) -> Promise<replyJson | object>`. A request is
  `{id, kind: "generate" | "chat", model, messages: [{role, content}], tools: [...],
  actions: [...]}`. A reply is `{content?, tool_calls?: [{name, arguments}], reasoning?,
  prompt_tokens?, completion_tokens?}` or `{error}`. `reasoning` is the thinking a model that
  reasons natively wrote before its answer (the page sends Qwen3's `<think>` block); NetGet
  never parses it, and forwards it to the status channel as `[REASONING]` lines, so the
  dashboard's stream shows it (`∴ …`) exactly as it shows an Ollama model's `thinking`.

  `actions` is every action the prompt offers, as data: `{name, description, tool, generic,
  parameters: [{name, type, description, required, choices?}], example, schema}`
  (`netget::llm::bridge::offered_action` documents each field). It is the only structured
  description a network event's request has — that path deliberately sends no native
  `tools` — and each `example` is one the action's own executor accepts. Nothing on the
  Ollama/OpenAI wire carries it.
- `start_server(json, cb)` — `cli::management::ServerForm`, the same path the dashboard's
  own form and MCP use.
- `start_client(json, cb)` — `cli::management::ClientForm`, as the dashboard's form and MCP
  `start_client`: `{"protocol":"http","remote_addr":"127.0.0.1:8090","instruction":"..."}`;
  `cb` gets `{id}` or `{error}`. `connect_client_to_server(serverId, cb)` is the dashboard's
  `[ + <proto> client ]` itself (`tui::actions::client_form_for_server` and the form's apply,
  default routing included); `cb` gets `{id, protocol, remote_addr}` or `{error}`.
  `send_to_client(id, actionJson, cb)` is the client card's `[ send ]`
  (`AppState::send_to_client`); `cb` gets the `ClientSendOutcome` as serde writes it
  (`{"Executed":{"detail":"http_request GET / -> 200 (5 byte body)"}}`, …) or `{error}`.
  `clients(cb)` lists `{id, protocol, remote_addr, status}`.
- `connect(port, onData, onClose) -> id`, `send(id, bytes)`, `close(id)` — a TCP client on
  the virtual network; `udp_open(onDatagram) -> id`, `udp_send(id, port, bytes)`,
  `udp_close(id)` — a UDP one. `listening_ports()`, `bound_udp_ports()` and `servers(cb)`
  describe what is there.

`site/js/demo.js` is the page, and the demo on it is Telnet only: three machines. On a wide
screen the Telnet client and the model sit side by side, the same height (the Telnet terminal
fills whatever the model panel makes the row, and never holds it open), with the dashboard
below them at full width; on a narrow one they stack as Telnet, dashboard, model. The page
around them carries no step list or notes: the machines are the explanation. Nothing needs a
click:

- About a second after `new NetGet(...)` the page calls `start_server` for a Telnet server on
  2323 with a short BBS instruction and one `llm` rule on `telnet_connection_opened` that
  asks for the welcome banner, so it is a normal instance on the dashboard (`rules 1`). The
  banner is a rule rather than a sentence in the instruction because every request carries
  the instruction and one event with nothing said before it: "when a visitor connects, send a
  banner asking for their name; after that, answer every line" had llama3.1:8b answer every
  typed line (`hello`, `hi there`, `what is this place?`, `play`) with the banner again, 20
  times in 20, and naming the events inside the instruction barely changed that (16 in 20).
  With the rule it answers `hello` and `hi there` with a greeting of its own (such as
  `Hello, how are you?`) 10 times in 10, and Gemini Nano with `Hi there!` / `Hello there!`.
  About a second later the Telnet terminal `connect()`s to it. The terminal reads as a shell session:
  a `$ ` prompt, `telnet localhost 2323` typed out so that it finishes as the client connects,
  then telnet(1)'s own `Trying 127.0.0.1...` / `Connected to localhost.` / `Escape character
  is '^]'.`; when the server hangs up, `Connection closed by foreign host.` and the prompt
  again, where Enter types the command again and reconnects. The client is line-mode: it
  edits and echoes the line locally (always; it refuses every option the server offers) and
  sends it whole on Enter. The connection's `telnet_connection_opened` is the first model
  request, so the greeting is the first thing anyone answers.
- Requests are answered one at a time and only the current one is on screen: the composer
  (below) while the visitor is the model, and what the model writes while a model answers.
  Its output streams into the LLM panel as it is generated (Prompt API `promptStreaming`,
  WebLLM's `stream: true` deltas), in fixed-height blocks that follow the newest line and
  scroll without a scrollbar, so the panel does not grow. A model that reasons natively
  (`thinks: true` in `WEBLLM_MODELS`: Qwen3 1.7B and 4B) gets a "Thinking…" block with its
  `<think>` text, then the answer; once answered the block folds to "Thought for N s ▸" and
  opens again on a click. Every other model (Gemini Nano, Qwen2.5, Llama 3.2, Hermes) shows
  its answer alone: it is not asked to explain itself, because a prompted explanation is not
  reasoning and would be mislabelled as such. `site/js/thinking.js` does the split. A model's
  answered request stays on screen, marked "answered in N s", until the next request
  replaces it; one the visitor answered goes idle at once.
- The model control is one `<select>` in the LLM machine's header. Where
  `LanguageModel.availability()` answers `available`, `downloadable` or `downloading`, its
  first option is the browser's built-in model, by name: "Gemini Nano (built into Chrome)",
  or "Phi-4-mini (built into Edge)" when the user agent says Edge (the API does not name its
  model; Edge's flag-gated Aion-1.0-Instruct cannot be told apart). Then the WebLLM models
  (`WEBLLM_MODELS`), each with its download size (the weights in the model's
  `ndarray-cache.json`; WebLLM's prebuilt config gives only VRAM) and "thinks" for Qwen3, or
  "downloaded" once WebLLM's
  `hasModelInCache` says so; without WebGPU they are listed disabled. The last option is
  "You are the model": chosen, the visitor answers every request even with a model loaded
  (which stays loaded, so choosing it again is immediate). The default is the built-in model,
  else the first WebLLM model, else "You are the model"; a choice made in the select,
  including that one, is kept in `localStorage` (`netget-demo-model`) and restored on the
  next visit.
  Choosing a model that is on this device (the built-in model `available`, or a cached WebLLM
  model) loads it and switches with no click; one that needs a download shows one button
  naming it and its size. The built-in model's download also starts on the visitor's first
  `pointerdown` or `keydown` anywhere on the page but the select (Chrome requires a user
  activation for it, and `create()` is called synchronously inside that handler so the
  activation counts), with `monitor`'s `downloadprogress` as the bar.

  Who answers a request (`route()` in `demo.js`): the model answering now, if there is one;
  otherwise, while the selected model is **on its way** — being looked for in the cache,
  starting (`LanguageModel.create()`), loading from the cache, or downloading once the
  visitor asked — the request **waits for it**, shown in the LLM panel as "waiting for Gemini
  Nano" / "Waiting for Gemini Nano to load… 40%", and goes to it the moment it is ready;
  otherwise (a model that needs a click to download, one that failed, or "You are the model")
  the visitor answers. This is what keeps the page-load race from landing on the visitor: the
  Telnet client connects about two seconds after NetGet boots, usually while the built-in
  model's session is still being created or a cached WebLLM model is still loading, and its
  connect request now waits for that model. If the load fails, a request that waited goes to
  the composer with the reason. A model already answering keeps answering while another
  loads. The status line under the select says which of these applies.

  The wait is bounded on the page, deliberately below NetGet's own bounds. NetGet waits for
  the page's answer to one request for `LLM_TIMEOUT` (900 s, `crates/netget-web/src/lib.rs`)
  and then fails it closed — for the connect event that means no banner, silently
  (`decision=connect_event_failed`) — and it hands the page one request at a time (the rate
  limiter's single permit), so a second network request waits behind the first for at most
  the limiter's queue timeout (300 s) and then fails. So a request waits for a loading model
  for `MODEL_WAIT_MS` (two minutes) and then goes to the visitor, with a note saying so. A model
  that becomes ready later still takes over any request the visitor has not started on, as it
  does when a download the visitor asked for finishes.

  On a switch the page calls `set_model` with the name of whoever answers. A WebLLM model
  that stops answering because another model took over is unloaded; the built-in model's
  session is kept, so switching back to it is immediate. The WebLLM runtime (esm.run) is only
  imported once a WebLLM model is selected.
- The built-in model gets a fresh session per request (NetGet sends the whole context each
  time) and `promptStreaming()` with a `responseConstraint`: a JSON Schema of `{"actions":
  [...]}` whose items are the offered non-tool actions, `type` pinned to each name, listed
  **first**, and exactly that action's parameters after it (`additionalProperties: false`).
  The order is load-bearing: a constrained decoder writes properties in schema order, so with
  `type` listed after the parameters a model that begins with `"type"` (as every example in
  the prompt does) could only reach the actions with no required parameter. Gemini Nano,
  llama3.1:8b and qwen2.5:1.5b all answered a typed "hello" with `send_telnet_prompt` and
  `"> "` that way, Nano under a half-written key (`"prompt__"`, `"promptłe"`) that the missing
  `additionalProperties: false` let through. Chrome's
  stream yields deltas; early versions yielded the whole text so far each time, and a chunk
  that extends what came before is taken as that. A session without `promptStreaming()` is
  asked through `prompt()`. Its text goes through the composer's own
  `entriesFromEnvelope`/`buildReply`, so an answer is accepted only if the composer could have
  built it; anything else sends that one request to the composer, with the reason. WebLLM's
  text goes back as written, to NetGet's own parser and repair, less a thinking model's
  `<think>` block, which goes as the reply's `reasoning` (a block with no `<think>` in sight,
  because the template opened it in the prompt, ends at `</think>`; one that never closes has
  no answer, and NetGet's retry takes it from there). Qwen3 is asked with
  `extra_body: {enable_thinking: true}` (WebLLM 0.2.85's toggle; `false` would prefill an empty
  think block), temperature 0.6 and top_p 0.95 (Qwen's own advice for thinking; greedy
  decoding makes it repeat itself), and `max_tokens` 2048, since its thinking is spent from the
  same budget. Its prebuilt context window is 4096 tokens; WebLLM refuses a prompt longer than
  that and stops generating at it.

The page never starts an HTTP-family server (see below for why they cannot answer here). The
dashboard's picker still lists every compiled protocol, so a visitor can.

When the visitor is the model, `site/js/composer.js` turns `actions` into a form, after the
dashboard's intercept composer: a picker of the offered actions (the protocol's own first
action preselected, not a `generic` one), one control per parameter by type, every field
prefilled from the action's example, "Add another action", "Answer with nothing" (`{"actions":
[]}`, a real answer), "Refuse (fail closed)", the schema behind a disclosure, and a Raw JSON
tab that mirrors the form and is sent verbatim when used. Its reply is the JSON envelope as
`content`, or `tool_calls` for a chat request whose entries are all native tools. A request
without `actions` gets the raw editor alone. The top half of the file is DOM-free so
`smoke.mjs` builds the same default reply under Node; `web/test/page_composer.py` drives the
page itself in headless Chromium: the Telnet server and client come up with no clicks, the
visitor answers through the composer and the answers reach the Telnet terminal, no element of
the demo has a scrollbar at 1280x800, 1440x900, 1920x1080 and 390x844, the terminal reads as
a telnet session through a hang-up and a reconnect, the removed explanatory text stays
removed, and a stub `LanguageModel` proves the Prompt API path both when the model is
`available` (named first in the select, loads by itself, answers with no composer,
constrained to the offered actions, falls back on an unparseable answer) and when it is
`downloadable` (waits for the first keypress). A stub whose `create()` the test holds for
seconds while the Telnet client connects proves the queue: the connect request shows as
waiting for the model, the composer never appears, and the model answers once `create()`
resolves; with `create()` failing instead, that request goes to the composer. "You are the
model" chosen with the stub loaded sends the next request to the composer, choosing the
model again hands requests back with no new session, and the choice survives a reload. Switching is
driven against a fake WebLLM module the test serves in place of the esm.run import: an
uncached model shows its sized download button and downloads nothing unasked, switching back
re-uses the built-in session, a cached model loads with no click, and the choice survives a
reload. Streaming is driven the same way: a stub whose `promptStreaming()` holds after two
chunks shows the partial answer in the panel before the Telnet terminal has it and never a
Thinking block (its second stream yields cumulative chunks, and the answer is the same), and
the fake WebLLM's Qwen3 streams a `<think>` block, held before `</think>`, which shows in the
Thinking block with no answer yet, folds to "Thought for N s" once answered, opens on a click,
and never reaches the Telnet terminal (with the real xterm.js the test reads it in the
dashboard's stream too). `smoke.mjs` checks `site/js/thinking.js` at each point of a `<think>`
stream and that a reply's `reasoning` reaches the dashboard.
Headless Chromium has no built-in model, so the stubs are the evidence for that path;
with Chrome itself the test also checks the real `availability()` is detected and no download
starts unasked. xterm.js is stubbed unless `XTERM_DIR` points at the real files; every other
non-local request is answered by the test. `SITE_DIR` serves another layout of the page
(deploy.sh's staged copy) instead of `site/`.

## Which protocols are in the browser build

Everything `crates/netget-web/Cargo.toml` lists: 68 features, every server protocol that
compiles for wasm32 — TCP ones (telnet, http, http2, ftp, pop3, nntp, smb, vnc, rdp,
modbus, kafka, nats, stomp, memcached, whois, gopher, finger, ident, svn, mercurial, sip,
rtsp, hls, snowflake, db2, mongodb-server, saml, openid, oci-registry, npm, pypi, maven,
elasticsearch, jsonrpc, oauth2, openapi, openidconnect, bitcoin, torrent-*, tls, …) and
UDP ones (udp, dhcp, dhcpv6, bootp, tftp, snmp, syslog, coap, radius, ssdp, netbios-ns,
rtp, gtp, hsrp, wol, dc, …). Eleven of those — http2, jsonrpc, npm, pypi, maven, oauth2,
openapi, openidconnect, elasticsearch, bitcoin, torrent-tracker — have a *client* that is
reqwest end to end; the client is gated with `not(target_arch = "wasm32")` in
`src/client/mod.rs` and the registry, the server is in. The ollama client is gated the same
way.

**The `http` client runs in the browser.** reqwest's wasm backend is the browser's `fetch`,
whose futures are not `Send` and which cannot reach the virtual loopback anyway, so on wasm32
`src/client/http/transport.rs` writes the request with hyper 1's `client::conn::http1` over
the shim's `TcpStream` — one connection per request, the body bounded at
`MAX_RESPONSE_BODY_BYTES` (8 MiB, the bound the native reqwest path enforces too) and the
exchange at `REQUEST_TIMEOUT` (30 s). hyper's *client* role never touches the clock, so the
date-cache panic below does not apply to it. **`https://` is refused** at connect with the
reason: the transport has no TLS, and nothing on the page's network holds a certificate a
client could verify. The transport compiles natively too, and
`tests/client/http/transport_test.rs` drives NetGet's HTTP and TCP servers through it (body,
headers, a 404, a chunked response, the body bound, the deadline). `web/test/smoke.mjs`
proves it in the bundle: `[ + http client ]` on an http card connects, and a client started
against NetGet's **HLS** server completes a model-driven exchange, a `[ send ]` and a 404.

Reusing the transport for the other eleven, measured by what each asks of reqwest:

- **jsonrpc, elasticsearch, openapi, bitcoin** — JSON over plain `send()`/`json()`/`text()`
  (bitcoin adds `basic_auth`, a header). Cheapest: swap the round trip for `transport::fetch`
  on wasm, as `http` does. Their servers are hyper-based, so a browser peer has to be one that
  is not (see below).
- **npm, pypi, maven** — the same, plus `bytes()` for artifacts: the transport would need a
  `Vec<u8>` body alongside the lossy `String`. Their default targets are public `https://`
  registries, which the browser build cannot reach at all.
- **torrent-tracker** — GET with a query and a **bencoded binary** body: needs the byte-body
  variant.
- **http2** — `http2_prior_knowledge()`: needs hyper's `client::conn::http2` with an executor,
  a second transport rather than a switch.
- **oauth2, openidconnect** — the `oauth2`/`openidconnect` crates call reqwest through their
  own `async_http_client`; each takes a custom HTTP function, so the transport can be plugged
  in, but it is an adapter per crate, and both flows assume `https://` issuers.

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
