# JSON-RPC 2.0 Server Implementation

## Overview

JSON-RPC 2.0 over HTTP POST. The LLM (or a script/static handler) implements every
method; there is no method registry and no response cache in Rust.

**Maturity**: `Experimental`. Single requests, batches and notifications work and are
verified against `curl`; the gaps below are known and listed.

## Protocol

- **JSON-RPC**: 2.0 (https://www.jsonrpc.org/specification)
- **Transport**: HTTP/1.1 POST, `hyper` 1, one tokio task per TCP connection
- **Content-Type**: `application/json` on responses

## Library choices

- **hyper** v1 — HTTP/1.1 framing, `Content-Length` and keep-alive
- **serde_json** — request parsing and response building
- **tokio** — runtime

No JSON-RPC crate: the specification is small, and a server-side implementation with
this action model would have to be written anyway.

## Request handling

### Correlation id — the response id always comes from the request

`call_llm_for_method` overwrites `id` on the outgoing response with the id parsed from
the request, unconditionally, preserving its JSON type. A numeric id comes back numeric,
a string id comes back a string. Neither the model nor a script can override it: an
invented id produces a reply the client cannot match, and over keep-alive that failure
is silent. `jsonrpc` is likewise forced to `"2.0"` on every response.

For this reason `jsonrpc_success` and `jsonrpc_error` **do not take an `id` parameter**.
They used to, described as "only set this explicitly if you need to override the default
behavior" — there is no such need.

### Notifications

Spec §4: a Notification is a request *without an `id` member*. An explicit `"id": null`
is a Request (discouraged, but valid) and is answered with `"id": null`.

- Single notification → HTTP 204, empty body.
- Notification inside a batch → no entry in the response array.
- A batch of nothing but notifications → HTTP 204, **not** `[]` (spec §6).

The event carries `is_notification` explicitly, because `id` cannot express the
difference: a missing id and an explicit null both serialise to `null` in event data.
A handler that answers a notification does no harm — the response is discarded.

### Response selection

`call_llm` executes every action the handler produced and returns them in
`protocol_results`. The server then **scans** those results for the one named
`jsonrpc_response` (unwrapping `ActionResult::Multiple`), rather than taking the first
raw action.

This matters: `raw_actions` includes common actions, so a response that leads with
`show_message` or `update_memory` — the exact shape this very document used to
recommend, and the shape the notification E2E test uses — was rejected as a "non-JSON-RPC
action" and turned into `-32603 Internal error`. Scanning also means the chosen action is
no longer executed a second time, which previously rendered every action log template
twice and recorded the pre-id-fill action in the MCP access log.

If no `jsonrpc_response` is produced, the client gets `-32603` with a message naming the
two actions it should have used.

### Batch requests

Processed sequentially, response order preserved. Non-object members (`[1,2,3]`) get
their own `-32600 / "id": null` entry per spec §6; they used to be dropped silently.

**Each batch member is a separate model call, so batch length is capped at
`MAX_BATCH_LEN` (128)** — an oversized batch is `-32600` with the limit named, refused
before any model is consulted. Batch length is a direct amplification factor on the LLM
backend and the body cap below does not bound it: at about forty bytes a member,
`{"jsonrpc":"2.0","method":"a","id":1}`, a 4 MiB body still buys a hundred thousand
sequential model calls from one unauthenticated POST. 128 is far above any real JSON-RPC
batch; anything genuinely batch-heavy belongs behind a script or static handler, which
costs no model call at all.

### Request body size

Capped at `MAX_REQUEST_BODY_BYTES` (4 MiB) via `http_body_util::Limited`; over the limit
is `-32600` naming the limit. `req.collect()` was unbounded — hyper imposes no limit of
its own — and the body is buffered whole, parsed into a `serde_json::Value` and then
pretty-printed into the trace log, so one client could grow the process without bound.

### Failure semantics

| Outcome | Caller receives | Log tag |
|---|---|---|
| Handler returns `jsonrpc_error` | that code and message, verbatim | — (the model meant it) |
| Handler runs but produces neither response action | `-32603` | `decision=model_no_answer` |
| LLM call errors, backend saturated | `-32000`, `data.retryable = true` | `decision=fail_closed_llm_overloaded` |
| LLM call errors, anything else | `-32603`, `data.retryable = false` | `decision=fail_closed_llm_error` |
| Body or batch over the cap | `-32600` naming the limit | `decision=reject_oversized[_batch]` |

The two failure codes are deliberately distinct: JSON-RPC 2.0 reserves -32000..=-32099 for
implementation-defined server errors, and reporting a transient rate-limiter refusal as
`-32603` tells the caller the server is broken when it is only busy. The peer-visible
`message` is `WireFailure::text()` — a `&'static str`, so the backend URL, the model name
and the `anyhow` context chain cannot reach the wire however the error is shaped; they go
to the log line instead. A notification is answered with silence whichever it was, which is
exactly why the tags matter: without them a swallowed failure left no trace at all.
Covered by `tests/server/jsonrpc/llm_failure_test.rs`.

## Actions

### `jsonrpc_success`

| Parameter | Required | Notes |
|---|---|---|
| `result` | yes | Any JSON value |

```json
{"type": "jsonrpc_success", "result": 8}
```

### `jsonrpc_error`

| Parameter | Required | Notes |
|---|---|---|
| `code` | yes | Integer, kept as `i64`. -32700 parse, -32600 invalid request, -32601 method not found, -32602 invalid params, -32603 internal, -32000..-32099 server |
| `message` | yes | Human-readable |
| `data` | no | Any JSON value |

```json
{"type": "jsonrpc_error", "code": -32601, "message": "Method not found"}
```

There are no async actions. `list_rpc_methods` used to be declared; it ignored its input,
always returned an empty list, and its result was consumed by nobody.

## Event: `jsonrpc_method_call`

| Field | Type | Notes |
|---|---|---|
| `method` | string | Method name |
| `params` | any | Array, object, or absent |
| `id` | string/number/null | Correlation id, original JSON type. Never needs echoing |
| `is_notification` | boolean | True when the request had no `id` member |

Static handlers can interpolate any of these with `{{event.field}}`.

## Examples

### Static handler (no model call)

```json
{"type": "open_server", "port": 8000, "base_stack": "jsonrpc",
 "event_handlers": [{"event_pattern": "jsonrpc_method_call",
   "handler": {"type": "static", "actions": [{"type": "jsonrpc_success", "result": {"ok": true}}]}}]}
```

Verified with `curl`:

```
$ curl -s -X POST http://127.0.0.1:8000/ -d '{"jsonrpc":"2.0","method":"add","params":[5,3],"id":"abc-123"}'
{"jsonrpc":"2.0","result":{"ok":true},"id":"abc-123"}

$ curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8000/ -d '{"jsonrpc":"2.0","method":"log"}'
204
```

### LLM mode

```
open_server port 8000 base_stack jsonrpc. JSON-RPC 2.0 server.
Implement add(a,b), greet(name) and version(). Return error -32601 for anything else.
```

The model answers each call with `jsonrpc_success` or `jsonrpc_error`; the id is handled
for it.

## Limitations

- **HTTP only** — no WebSocket or raw TCP transport.
- **No authentication and no rate limiting.** Body size and batch length are capped
  (above); nothing else is.
- **No routing** — every path is a JSON-RPC endpoint; there is no 404.
- **Non-POST** returns HTTP 200 with an `-32600` body rather than `405 Method Not
  Allowed`, so a plain `GET /` looks like a working endpoint to a scanner.
- **No `Content-Type` validation** on requests; any body is parsed as JSON.
- **No per-request timeout** — a slow body holds a connection task until the cap is hit.
- **Notifications still cost a model call** whose output is discarded. Deliberate (it
  keeps logging and memory updates working), but it is not free.
- **Per-connection tasks are untracked**, so `stop_server` does not abort in-flight
  requests. Only the accept loop is registered with `AppState::register_server_task`.
- **Byte and packet counters are never updated**, so connection stats read zero.
- `track_method_call` is **gone**. It maintained a ten-entry `recent_methods` ring inside
  the connection's `protocol_info` that nothing in the tree read — not the dashboard, which
  reaches `protocol_info` only through IMAP-specific accessors, not the MCP surface, not a
  test — and it cost a write lock on the single global `AppState` `RwLock` on every request
  plus a clone-and-reparse of the vector. The method name is already in the access log and
  in the event's log template.

## References

- [JSON-RPC 2.0 Specification](https://www.jsonrpc.org/specification)
- Testing notes: `tests/server/jsonrpc/CLAUDE.md`
