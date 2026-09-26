# HTTP/2 Protocol Implementation

HTTP/2 server built directly on the `h2` crate. `h2` owns framing, HPACK,
multiplexing and flow control; the LLM owns the response status, headers and text
body, plus optional server pushes.

**State**: Experimental — not human-reviewed against a broad client set.
**Privilege**: declares `PrivilegedPort(443)`; the check fires only when the
requested port is actually below 1024. **RFC**: 7540 / 7541.

Shared plumbing (request extraction, response building, request filter) lives in
`src/server/http_common/` — read `src/server/http_common/CLAUDE.md`.

## One server — `h2_server.rs`

`Http2Protocol::spawn()` calls `H2Server::spawn_with_push_support`
(`h2_server.rs`), because hyper's service API cannot express server push. All
request handling lives there; `mod.rs` is module declarations and the `H2Server`
re-export only.

There used to be a second, hyper-based `Http2Server` in `mod.rs`. It compiled and
was exported as `server::Http2Server`, but nothing ever routed traffic to it, so
edits to it had no effect on a running server. That bit the request filter: it
was wired only into the dead path, so `request_filter` was accepted and silently
ignored for HTTP/2 until it was added to `h2_server.rs` as well. The dead server
was removed rather than kept as a "reference implementation" — a second copy of
request handling that no test and no client ever exercises only invites the same
mistake again. **Do not reintroduce a second server here.**

The `h2c` upgrade path from HTTP/1.1 (`src/server/http/mod.rs`) also lands in
`h2_server::handle_h2_request`, carrying the HTTP/1.1 connection's filter with it.

## What the model sees and controls

**Event**: `http2_request`, one per HTTP/2 stream.

`method`, `uri` (path+query), `version`, `headers` (lowercase map), `body` (UTF-8
lossy), `body_bytes`, and `body_is_binary` (present and `true` only when the body
was not valid UTF-8).

**Actions**:

- `send_http2_response` — `status` (required, 100-599), `headers`, `body`
  (optional; omit for 204/304). Same executor as HTTP/1.1.
- `push_resource` — `path` (required), `status`, `headers`, `body`. Emit it in
  the same batch as the response; pushes are sent as PUSH_PROMISE + a push stream
  **before** the main response. A client that has disabled push (most modern
  browsers) rejects it; the push is then dropped with a warning and only the main
  response is delivered.

Same hard limits as HTTP/1.1: **one response per stream, sent complete; no
streaming or chunking; text bodies only, no binary payloads;** request bodies are
buffered whole, bounded by `http_common::MAX_REQUEST_BODY_BYTES` (8 MiB) — a larger
one is answered `413` and never reaches the model. That bound matters more here than
on HTTP/1.1: `release_capacity` re-opens the flow-control window after every chunk,
so without a *total* limit a peer can stream an unbounded amount into the buffer.
Do not set HTTP/2 pseudo-headers (`:status`, `:path`, …) or `content-length` — `h2`
handles them; illegal header names/values are dropped rather than sent.

### Failure behavior

- LLM error → the *category* reaches the client, never the error text
  (`crate::utils::WireFailure`): an **overloaded** backend → `503` + `Retry-After: 1`,
  anything else → `500`. Logged as `decision=fail_closed_llm_overloaded` /
  `decision=fail_closed_llm_error`. Same split as HTTP/1.1's `build_error_response`;
  HTTP/2 answered a flat 500 for both until this pass, so a client could not tell
  "come back in a second" from "this is broken".
- No response action → the server's `default_response` startup param if set, otherwise
  a fail-closed `500` (`decision=fail_closed_no_action`) carrying the same category
  body. It is deliberately *not* an empty `200`, which a client cannot tell from a real,
  empty answer. `default_response` is declared by
  `request_handling_startup_parameters()`, which HTTP/2 advertises; until this pass
  HTTP/2 read it nowhere, so setting it changed nothing.
- Request body over the 8 MiB cap → `413`, `decision=refused_body_too_large`, no LLM
  call.
- A body that would take its connection past the shared 8 MiB body budget → `503` +
  `Retry-After: 1`, `decision=refused_connection_body_budget`, no LLM call (see Stream bounds).
- A stream past `MAX_CONCURRENT_STREAMS` → `RST_STREAM(REFUSED_STREAM)` from `h2`, no event.
- Invalid status/header from the model → 500 / header dropped
  (`build_h2_response_head`), never a panic and never a dead stream.
- Errors from `send_response`/`send_data` propagate out of `handle_h2_request`
  and are logged; the peer sees a reset stream.

## Architecture

- `H2Server::spawn_with_push_support` binds through
  `create_reusable_tcp_listener`, propagates bind failure, and registers the
  accept-loop `JoinHandle` via `AppState::register_server_task()` so
  `stop_server` releases the socket.
- Optional TLS via `tls_cert_manager` (`tokio_rustls` in front of the `h2`
  handshake). **ALPN is never advertised**, so a browser will not select HTTP/2
  over TLS on its own — clients must pick `h2` explicitly, or use cleartext h2c.
- One task per connection, one task per stream, both spawned through
  `AppState::spawn_server_task` so `stop_server` aborts open connections and answers in
  progress, not only the accept loop. Streams on a connection are
  processed concurrently; how many *model* calls run at once is bounded by
  `--llm-max-concurrent` (default 1). Do not reason about this from `--ollama-lock` —
  that flag is accepted and inert, and its plumbing was deleted.
- The request filter is built **once per server** in
  `spawn_with_push_support` (not per connection), and
  `RequestFilter::warnings()` is forwarded to the status channel — parsing is
  fail-open, so a typo means more LLM traffic, not less.
- Handling mode priority is the generic one: script → static → LLM, through
  `call_llm` → `try_execute_event_handler`.

### Connection bounds

All four live in `h2_server.rs` beside their arguments. The two read bounds are declared startup
parameters, so the operator can change them.

| Bound | Default | Parameter | Mechanism |
|---|---|---|---|
| First byte | 30s | `first_byte_timeout_secs` | `TcpStream::peek` with a deadline, before rustls or `h2` sees the socket |
| TLS handshake + preface | 30s | — | `timeout` around `TlsAcceptor::accept` and `server::handshake`; no model and no person is involved in this phase |
| Idle between requests | 300s | `idle_timeout_secs` | `watch_idle` over a `ConnectionActivity` raced against `accept()`; on expiry a GOAWAY (`graceful_shutdown`), then close |
| Connections | 128 | — | `accept_bounded`; h2c peers over the cap read an HTTP/1.1 `503` + `Retry-After`, TLS peers a plain close |

- **Busy is not idle.** Every stream's task holds the connection's `ConnectionActivity` busy for
  the whole of its answer, so a request waiting on the model or parked for a human by a
  `manual` rule never lets the idle watchdog fire. PING frames are answered by `h2` and are
  not counted as activity.
- **Which client state the first-byte bound faces: lazy.** NetGet's own HTTP/2 client is a
  pooled `reqwest` client with `http2_prior_knowledge()`; it opens no socket until it has a
  request to send, so it is never connected and silent. 300s idle stays above the 90 seconds
  that pool keeps an idle connection.

### Stream bounds

`h2` advertises no stream limit and a 16 MiB header list unless told otherwise, so every
connection is handshaken with `bounded_h2_builder()` — prior-knowledge h2c, TLS, and the
HTTP/1.1 `Upgrade: h2c` path in `src/server/http/mod.rs` alike. Each value is argued beside its
constant in `h2_server.rs`.

| SETTINGS / bound | Value | Why |
|---|---|---|
| `MAX_CONCURRENT_STREAMS` | 100 | RFC 9113 §6.5.2's recommended floor, Apache's default. A stream past it is reset by `h2` with `REFUSED_STREAM` (safe to retry) |
| `INITIAL_WINDOW_SIZE` | 65,535 | the protocol default, stated: bounds what `h2` holds for a stream this server has stopped reading |
| connection window | 1 MiB | a `WINDOW_UPDATE` on stream 0 opens it past 64 KiB so 100 uploads do not serialise; bounds unread body bytes across all streams |
| `MAX_FRAME_SIZE` | 16,384 | the protocol minimum; nothing a request carries needs more |
| `MAX_HEADER_LIST_SIZE` | 32 KiB | nginx's HTTP/1.1 equivalent; `h2`'s 16 MiB default × 100 streams was 1.6 GiB |
| body budget per connection | 8 MiB | see below |

**One body budget per connection, not one per stream.** A stream buffers its body whole, up to
`http_common::MAX_REQUEST_BODY_BYTES` (8 MiB). Per stream alone that is 800 MiB a connection at
100 streams. So the streams of a connection share one 8 MiB `BodyBudget`: a single upload can
still use all of it, and a stream whose body would take the connection past it is answered
**503** + `Retry-After: 1` with `decision=refused_connection_body_budget`, before the model sees
it. Its bytes are released at once; every other stream's when its answer has been sent. The 413
for one body over 8 MiB is unchanged. Per connection that is 8 MiB of bodies + 1 MiB of unread
data in `h2` + 100 × 32 KiB of headers ≈ 12 MiB, ≈ 1.5 GiB across 128 connections.

Not bounded here: what the server *sends*. `h2`'s per-stream send buffer
(`max_send_buffer_size`, 400 KiB) is left at its default, and a response body comes from the
model or a handler rather than from the peer.

### Connection state

One `ConnectionId` per TCP connection (**not** per stream). Statistics are
therefore recorded against the connection from inside `handle_h2_request`, which
runs per stream: every request counts one message received (body bytes only —
`h2` has consumed the HEADERS frame by then), and every response, server push and
filter rejection counts one message sent. Requests rejected by the filter are
counted too, so a filtered connection still refreshes `last_activity`.

This matters more here than for HTTP/1.1: `cleanup_old_connections`
(`src/state/server.rs`) evicts any connection idle for 10s, and an HTTP/2
connection routinely lives far longer. See `src/server/http/CLAUDE.md` for the
full rationale and for who reads the counters.

**Still a gap**: `ProtocolConnectionInfo` is initialized to
`{"recent_requests": []}` and never appended to; nothing reads it and no accessor
exists to write it.

## Testing

`tests/server/http2/e2e_test.rs` — 3 mocked scenarios (basic GETs, POST with body,
multiplexing), driven with `reqwest`'s `http2_prior_knowledge()`.
`tests/server/http2/failure_semantics_test.rs` — 2 more: `500` on backend failure with
no internal detail and no `Retry-After` in the body, and `413` (proven to cost no LLM
call) for a body over the size cap.
`tests/server/http2/connection_bounds_test.rs` — the four connection bounds above, from the
peer's side, in-process and model-free.
`tests/server/http2/stream_bounds_test.rs` — the stream bounds: the SETTINGS frame read by a
hand-written frame reader and by curl (nghttp2), a 101st stream refused with `REFUSED_STREAM`
while the first 100 park, the shared body budget answering `503`, and (with `http` compiled in)
the same stream limit on the h2c upgrade path. All four files are declared in
`tests/server/http2/mod.rs`.

```bash
./cargo-isolated.sh test --no-default-features --features http2 \
    --test server::http2::e2e_test -- --test-threads=100
```

**Gaps**: no coverage of server push, of the request filter on the HTTP/2 path, of
`default_response`, of TLS, or of the h2c upgrade from HTTP/1.1.

**On maturity**: HTTP/2 stays **Experimental**, and a push test would not change that.
Root `CLAUDE.md` lists `http2` under "evidence is only a generic HTTP client", and the
obvious way to test push — the `h2` crate's client `push_promises()` — is the circular
case that file also names: the server frames with `h2` too, so it would assert only
that one crate round-trips through itself. Promotion needs a peer that is not `h2`
(curl, nghttp2, a browser) completing a real exchange, including ALPN, which this
server does not advertise.

## Example prompts

```
listen on port 8080 via http2
For GET /, return JSON {"message": "Hello HTTP/2"}
For POST /api/users, parse the JSON body and return 201
```

Deterministic variant (no LLM call):

```json
"event_handlers": [{
  "event_pattern": "http2_request",
  "handler": { "type": "static", "actions": [
    { "type": "send_http2_response", "status": 200,
      "headers": {"Content-Type": "application/json"},
      "body": "{\"message\": \"Hello from HTTP/2!\"}" }
  ]}
}]
```

## Not implemented

ALPN negotiation · stream prioritization control · streaming/chunked responses ·
binary bodies · LLM control over connection lifetime · trailers · `recent_requests`.

## References

- RFC 7540 (HTTP/2), RFC 7541 (HPACK)
- [h2](https://docs.rs/h2/latest/h2/)
