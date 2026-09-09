# HTTP/1.1 Protocol Implementation

HTTP/1.1 server built on hyper v1.0. Hyper owns the protocol (parsing,
keep-alive, chunked *request* decoding, connection management); the LLM owns one
thing only — the response status, headers and text body.

**State**: Beta — human-reviewed, verified against real clients (`curl`,
`reqwest`). **Privilege**: declares `PrivilegedPort(80)`; the preflight check
fires only when the requested port is actually below 1024.
**RFC**: 7230-7235.

Shared request/response plumbing lives in `src/server/http_common/` — read
`src/server/http_common/CLAUDE.md` too; the filter and response-building
contracts are documented there, not repeated here.

## What the model sees and controls

**Event**: `http_request`, one per HTTP request (not per TCP connection).

| Field | Notes |
|---|---|
| `method` | `GET`, `POST`, … |
| `path` | path only, no query string |
| `query_string` | raw string, present only when there was one |
| `query` | parsed, URL-decoded key→value object |
| `headers` | lowercase name→value object; non-UTF-8 values are dropped |
| `body` | request body decoded as UTF-8 **lossily** |
| `body_bytes` | body size in bytes before decoding |
| `body_is_binary` | present and `true` only when the body was not valid UTF-8 |

**Action**: `send_http_response` — the only one. `status` (required, 100-599),
`headers` (optional), `body` (optional; omit for 204/304).

There are no async actions: HTTP is purely reactive.

### Hard limits, stated in the action description so the model sees them too

- **One response per request, sent complete.** No chunked/streaming responses, no
  server-sent events, no way to hold the request open or send in parts.
- **No binary response bodies.** `body` is a string written as UTF-8. Images,
  gzip, protobuf cannot be produced. There is no hex/base64 escape hatch, by
  design: action parameters must not carry encoded bytes.
- **Non-UTF-8 request bodies are lossy.** The raw bytes are not exposed to the
  model in any form; `body_is_binary` tells it the text is not the real payload.
- **Request bodies are fully buffered**, bounded by
  `http_common::MAX_REQUEST_BODY_BYTES` (8 MiB), before the LLM call. A larger body is
  refused with `413` and costs no LLM call — it is refused rather than truncated,
  because a truncated body is indistinguishable from a complete one to the model.
- `Content-Length` and `Date` are set by hyper; setting them in `headers` is
  ignored or harmful. Header names/values that are not legal HTTP (e.g.
  containing CR/LF) are dropped rather than injected.

### Failure behavior

- LLM call fails → the *category* reaches the client, never the error text
  (`crate::utils::WireFailure`). An **overloaded** backend → `503` + `Retry-After: 1`,
  so the client backs off instead of recording a permanent fault; anything else → `500`.
  The log carries `decision=fail_closed_llm_overloaded` / `decision=fail_closed_llm_error`
  alongside the full error. This is the behaviour root `CLAUDE.md` tells other protocols
  to copy; `tests/server/http/failure_semantics_test.rs` pins it.
- Model emits no `send_http_response` → the server's `default_response` startup param
  if set, otherwise an empty `200`. The `send_http_response` action and `http_request`
  event descriptions now tell the model to always answer and to honor the client's
  `Accept` header (return a matching `Content-Type`; 404 for an image/binary it cannot
  produce), and the `request_filter` param is recommended so favicon/preflight noise
  never reaches the model.
- Model emits a `send_http_response` the executor rejects (e.g. a status outside
  100-599) → the action is dropped with a warning by
  `execute_actions()` and the client still gets the empty `200`. This is why the
  executor is lenient about status/body shapes; see `http_common/CLAUDE.md`.
- A status or header hyper cannot represent → 500 / dropped header, never a panic.

## Architecture

- `spawn_with_llm_actions` binds via `create_reusable_tcp_listener`, propagates
  bind failure with `?` (so `server_startup` reports `Error` rather than a
  phantom `Running`), and registers the accept-loop `JoinHandle` with
  `AppState::register_server_task()` so `stop_server` releases the socket.
- Optional TLS: `startup_params` are parsed by
  `crate::server::tls_cert_manager::extract_tls_config_from_params`; when
  present, each accepted stream goes through `tokio_rustls` before hyper. The
  server logs itself as `HTTPS` in that case. (ALPN is not advertised.)
- One `tokio` task per TCP connection; hyper's `service_fn` calls the LLM per
  request. Per-connection tasks are not tracked, so `stop_server` does not cancel
  in-flight requests.
- Handling mode priority is the generic one: script handler → static handler →
  LLM (`call_llm` → `try_execute_event_handler`). Script and static handlers cost
  no LLM call.
- `h2c` upgrade: an `Upgrade: h2c` request with an `HTTP2-Settings` header gets
  `101 Switching Protocols` and the connection is handed to
  `http2::h2_server::handle_h2_request` (feature-gated on `http2`; without it the
  server answers `501`). The request filter is carried across the upgrade.

### Connection state

One `ConnectionId` per TCP connection, added on accept and closed when the socket
closes.

`bytes_*`, `packets_*` and `last_activity` are maintained per request by the
wrapper around `handle_http_request_inner`, on **every** exit path — including
h2c upgrade and request-filter rejections, which never reach the model.
Semantics: one "packet" = one HTTP message; byte counts are **message bodies
only**, because hyper has parsed the request line and headers away before the
server sees them, and `Full<Bytes>::size_hint()` only knows the response body.
So the numbers track payload volume, not wire volume.

Two things read them, and both were broken while they stayed at zero:

- `ServerInstance::cleanup_old_connections` (`src/state/server.rs`) drops any
  connection whose `last_activity` is older than 10s; the TUI and the MCP loop
  both call it on a timer. Without the refresh, a keep-alive connection was
  evicted from the state map 10s after it opened while it was still serving, and
  every later stat update and the final `close_connection_on_server` targeted a
  connection that no longer existed.
- Connection-scoped scheduled tasks put the counters and the idle time directly
  into the model's prompt (`src/llm/prompt.rs`), so idle-timeout and
  rate-limiting instructions were reasoning about constant zeros.

The TUI's connection list shows only id/address/state
(`ConnectionDisplayInfo`, `src/ui/app.rs`), so these counters are **not** on
screen; they reach the model and the cleanup timer instead.

**Still a gap**: `ProtocolConnectionInfo` is initialized to
`{"recent_requests": []}` and is never appended to. Nothing in the codebase reads
`recent_requests`, and there is no accessor to push into it — populating it needs
a new method on `AppState`/`ServerInstance` first, or the field should be dropped.

## Request filtering

By default every request costs an LLM round-trip, including favicon probes, CORS
preflights and scanner noise. `request_filter` in `startup_params` is an
allowlist: a request reaches the LLM only if it matches at least one rule;
everything else gets `filtered_response` (default 404) with no LLM call.

```json
"startup_params": {
  "request_filter": [ { "methods": ["GET"], "headers": { "accept": "text/html" } } ],
  "filtered_response": { "status": 404, "body": "Not Found" }
}
```

That one rule covers the common noise generically — favicon requests carry
`Accept: image/*` and preflights are not `GET`. It replaced an earlier hardcoded
favicon bypass.

Full schema, matching semantics and the **fail-open** caveat (a malformed rule is
dropped, not fatal, so a typo sends *more* traffic to the LLM) are in
`src/server/http_common/CLAUDE.md`. The filter is built once at spawn time; parse
problems are logged at `error!` and pushed to the status stream as
`[ERROR] HTTP request_filter: …`, so they show up in the `start_server` result. Pure unit tests: `tests/http_request_filter_test.rs`.

## Testing

- `tests/server/http/test.rs` — 7 mocked E2E scenarios (simple GET, JSON API,
  routing, headers, methods, error responses, logging) driven through the real
  binary with `reqwest`. Declared in `tests/server/http/mod.rs`.
- `tests/server/http/failure_semantics_test.rs` — 3 scenarios: 500 on backend failure,
  no internal detail in the failure body, and 413 (with no LLM call) for a body over the
  size cap.
- `tests/server/http/e2e_scheduled_tasks_test.rs` — scheduled-task coverage.
- `tests/http_request_filter_test.rs` — pure filter unit tests.

```bash
./cargo-isolated.sh test --no-default-features --features http \
    --test server::http::test -- --test-threads=100
```

**Gaps**: no test covers TLS/HTTPS, the h2c upgrade path, the request filter
end-to-end through a running server (only the pure unit tests), a non-UTF-8
request body, a model response with an invalid status/header, or the `503` +
`Retry-After` branch specifically (the mock's HTTP 500 classifies as `Unavailable`,
so the tests exercise the `500` side; provoking an overload needs a saturated
rate limiter).

## Example prompts

```
listen on port 8080 via http
For GET /, return <h1>Welcome</h1>
For GET /about, return <h1>About Us</h1>
For other paths, return 404 with "Not Found"
```

```
listen on port 3000 via http
For POST /api/users, parse the JSON body and return 201 with
Content-Type: application/json and body {"status":"created","id":123}
```

```
listen on port 8080 via http
For GET /health, return 200 with body: OK
For GET /redirect, return 301 with Location: /home
For DELETE /items/*, return 204 with no body
```

For deterministic behavior prefer a static or script handler over the
instruction — same result, no LLM call:

```json
"event_handlers": [{
  "event_pattern": "http_request",
  "handler": { "type": "static", "actions": [
    { "type": "send_http_response", "status": 200,
      "headers": {"Content-Type": "text/plain"}, "body": "Hello World" }
  ]}
}]
```

## Performance

One LLM call per request unless a script/static handler matches or the request is
filtered out; 2-5 s per call with `qwen3-coder:30b`. Requests are handled
concurrently (a task each); how many model calls run at once is bounded by
`--llm-max-concurrent` (default 1, so throughput is effectively one request at a time),
with `--llm-queue-timeout` and `--llm-max-queued` bounding the wait. Do **not** reason
about this from `--ollama-lock` — that flag is accepted and inert, and its plumbing was
deleted. Keep-alive avoids repeated TCP handshakes. Everything is buffered in memory.

## Not implemented

WebSocket upgrade · streaming/chunked responses · request-body streaming ·
binary bodies · multipart/urlencoded form parsing (the body is handed over raw) ·
LLM control over keep-alive or connection close · ALPN · `recent_requests`.

## References

- RFC 7230-7235 (HTTP/1.1)
- [hyper](https://docs.rs/hyper/latest/hyper/)
