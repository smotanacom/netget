# MCP (Model Context Protocol) Server Implementation

An MCP server whose resources, tools and prompts are all supplied by the handler. JSON-RPC 2.0
over a single HTTP POST endpoint.

**State**: `Experimental` · **Default port**: 8000 · **Stack**: `ETH>IP>TCP>HTTP>MCP`

> Not to be confused with `src/mcp_stdio/`, which is NetGet's *own* MCP server (the `--mcp`
> flag). This module is the MCP *protocol implementation* that NetGet can serve to others.

## Libraries

- **axum** 0.7 — one route, `POST /`.
- Hand-written JSON-RPC 2.0 in `jsonrpc.rs`.

## No storage

The handler answers every request. **No tool, resource or prompt is defined in Rust** —
`tools/list` and `tools/call` are entirely handler-driven, with hardcoded fallbacks of
`{"tools": []}` and an error.

A `session.rs` used to sit alongside this holding an `McpSession` with `initialized`,
`capabilities`, `subscriptions`, and `tools`/`resources`/`prompts` maps. It has been deleted.
It was two problems at once:

- a protocol-level store of tools/resources/prompts, which the no-storage rule forbids; and
- an unbounded leak — every `initialize` from any unauthenticated client inserted an entry, and
  the map had no remove, no expiry and no cap.

It was also entirely dead: the map was written here and read nowhere in the tree, and every
mutator (`mark_initialized`, `subscribe`, `register_tool`, …) had zero call sites. Removing it
regresses nothing, which is why it was removed rather than left half-built.

## Methods

Handler-driven, each with its own event:

| JSON-RPC method | Event | Action |
|---|---|---|
| `initialize` | `mcp_initialize` | `mcp_initialize_response` |
| `resources/list` | `mcp_resources_list` | `mcp_resources_list_response` |
| `resources/read` | `mcp_resources_read` | `mcp_resources_read_response` |
| `tools/list` | `mcp_tools_list` | `mcp_tools_list_response` |
| `tools/call` | `mcp_tools_call` | `mcp_tools_call_response` |
| `prompts/list` | `mcp_prompts_list` | `mcp_prompts_list_response` |
| `prompts/get` | `mcp_prompts_get` | `mcp_prompts_get_response` |

`mcp_error_response` is offered on all seven.

Routed but **answered without consulting the handler** — worth knowing before writing an
instruction that assumes otherwise:

`ping` → `{}` · `resources/subscribe` → `{}` (the URI is logged and discarded) ·
`resources/unsubscribe` → `{}` · `resources/templates/list` → `{"resourceTemplates": []}` ·
`logging/setLevel` → `{}` (the level is not applied) · `completion/complete` → an empty
completion.

Notifications (`notifications/initialized`, `.../cancelled`, `.../progress`) are logged only.
Nothing is actually cancelled.

### The action name is `*_response`

The action is `mcp_initialize_response`; `mcp_initialize` is the *event id* and the internal
`ActionResult` name. An earlier version of this document showed `{"type": "mcp_initialize"}`,
`{"type": "mcp_resources_list"}` and `{"type": "mcp_tools_call"}` in its three worked
examples — none of those are actions, and `execute_action` rejects them with "Unknown MCP
action".

Relatedly, all sixteen event types used to carry `{"type": "placeholder", "event_id": …}` as
their `response_example`. That field is rendered verbatim into the model's prompt and into
`get_protocol_docs` output as *the* way to answer the event, and `"placeholder"` is not an
action, so a model following the example failed every time. Every event now carries a real,
correctly named example.

## Errors

`mcp_error_response` takes `code`, `message` and optional `data`, and now actually produces the
JSON-RPC error. Nothing used to consume its result: every handler loop matched one action name
and ignored the rest, so a chosen error was dropped and the caller received either a generic
`-32603` or — on `tools/list`, `resources/list` and `prompts/list` — a **success** reply of
`{"tools": []}` / `{"resources": []}` / `{"prompts": []}`. The script handler shipped in
`get_startup_examples`, which ends `action('mcp_error_response', code=-32601, ...)`, could not
work before this.

### Three outcomes, three `decision=` tags

An operator has to be able to tell apart a model that refused, a model that said nothing, and a
backend that broke. All three are logged (tracing + the status stream) with a stable tag:

| Outcome | Caller receives | Log tag |
|---|---|---|
| Handler returns `mcp_error_response` | that JSON-RPC error, verbatim | `decision=model_reject` |
| Handler runs but produces no usable action | the per-method default (see Methods) | `decision=model_no_answer` |
| LLM call errors, backend saturated | `-32000`, `data.retryable = true` | `decision=fail_closed_llm_error_overloaded` |
| LLM call errors, anything else | `-32603`, `data.retryable = false` | `decision=fail_closed_llm_error_unavailable` |

The two failure codes are deliberately distinct so a client backs off on saturation instead of
recording a permanent fault. The peer-visible `message` is `WireFailure::text()` — a
`&'static str`, so the backend URL, the model name and the `anyhow` context chain cannot reach
the wire no matter how the error is shaped. They go to the log line above instead.

## Correlation

The JSON-RPC `id` is echoed on every reply, success and error alike; `handle_jsonrpc` clones it
before the request is consumed and re-attaches it. Handlers only supply the `result` body, so
they cannot get it wrong and nothing id-related needs to reach them.

A parse failure now also echoes the id when it can be recovered from the raw payload
(`recover_request_id`). It previously passed `None` unconditionally, so a request with a missing
`jsonrpc` field or a non-string `method` came back with `"id": null` even though the id was
sitting right there — leaving the client unable to match the error to its request, which the
spec requires.

`RequestId` covers strings and `i64` numbers; `null` round-trips correctly. A float id, or one
larger than `i64::MAX`, still fails to parse and loses correlation. Malformed JSON never reaches
this code — axum rejects it with an HTTP 400 and a plain-text body rather than a JSON-RPC
`-32700`, so `ErrorCode::ParseError` is unreachable.

## Robustness

- **No `unwrap()`, `expect()`, slicing, or signed-to-`usize` casts anywhere in this module.**
  All framing is axum's; there is no hand-rolled length prefix or line parser.
- **Trace output is capped** at 4 KiB and truncated on a char boundary. The entire request body
  used to be serialized onto `status_tx` on every call — an unbounded channel with no
  backpressure — so a client posting 2 MiB bodies could enqueue faster than the TUI drains.
- **Connections no longer leak.** `initialize` registers a connection for visibility and marks
  it closed on every exit path. Each was previously left `Active` forever, so repeating
  `initialize` grew `AppState` without bound.
- **Protocol version is negotiated**, not hardcoded. The fallback response echoes the client's
  requested revision if it is one of `2024-11-05`, `2025-03-26`, `2025-06-18`, and otherwise
  offers `2024-11-05`. It used to answer `2024-11-05` unconditionally, telling a client on a
  newer revision that its request had been honored.
- **Body size is capped at `MAX_REQUEST_BODY_BYTES` (2 MiB), set explicitly** on the router as
  `DefaultBodyLimit::max(..)` and declared as `max_inbound_bytes`. It is the same number as
  axum's own default, but a framework default is a number nobody chose. It is enforced while the
  body is buffered — up front from a `Content-Length`, as it streams for a chunked body — so an
  over-limit request never reaches the JSON parser or the model. The handler takes
  `Result<Json<Value>, JsonRejection>` so the refusal is its own: HTTP 413 carrying a JSON-RPC
  error (`-32600`, fixed message `request body too large`, `id: null`), logged
  `decision=fail_closed_body_too_large`. Other rejections (wrong content type, bad JSON) stay
  axum's. `serde_json`'s 128-level recursion limit is what stops deep nesting.
  `tests/server/mcp/inbound_limit_test.rs` drives it from the wire.
- Bind uses `?`; `axum::serve`'s handle is registered via `register_server_task()`, so
  `stop_server` releases the port.
- LLM failures return a JSON-RPC error with the request `id` echoed, rather than leaving the
  caller hanging. All seven handlers route through `llm_failure_error`, so the shape is
  identical across them: `-32603` (InternalError) normally, and `-32000` with
  `"retryable": true` when `crate::llm::is_overload_error` says the backend is merely at
  capacity — JSON-RPC 2.0 reserves -32000..=-32099 for exactly that. Reporting overload as
  -32603 would tell the caller the server is broken when it is only busy. Covered by
  `tests/server/mcp/llm_failure_test.rs`.

## Known limitations

- **HTTP POST only.** There is no `GET /sse`, so a client using the 2024-11-05 HTTP+SSE
  transport gets a 405 and cannot connect at all. Use a Streamable-HTTP transport.
- **Notifications answer 204**, where the spec prescribes 202. A 204 carries no `Content-Type`,
  which some SDK transports reject.
- **No batch requests.** An array payload has no `id`, so it is treated as a notification, fails
  to parse, and returns `-32600`.
- **The handshake is decorative.** `notifications/initialized` is logged and nothing is gated on
  it — every method is servable before `initialize`. No session id is returned to the client
  (no `Mcp-Session-Id` header), so a session could not be referenced even if one were kept.
- **`initialize` is the only method that passes a `connection_id`** to `call_llm`; the other six
  pass `None`, so per-connection access logging is inactive for them.
- **Nothing is retained between calls.** A `tools/list` has no memory of what `initialize`
  declared; consistency across calls comes from the instruction, not from state.
- `mcp_resources_subscribe_response` and `mcp_completion_response`, and the
  `mcp_resources_subscribe` and `mcp_completion` event types, have been **removed**. No
  `Event::new` ever fired them, so an `event_pattern` naming either got a handler that could
  never run. `get_mcp_event_types()` also used to build a second, independent copy of every
  event by hand, without log templates and free to drift; it now returns the same statics
  `mod.rs` emits.

### base64

No action parameter carries raw bytes or hex. But MCP's own wire format puts base64 in two
places the handler must produce: a resource's `blob` and image content in a tool result
(`{"type": "image", "data": "<base64>", ...}`). The server passes the handler's `response`
object through verbatim, so there is no encode/decode asymmetry — but small models emit base64
poorly, and neither action's example shows those forms. Prefer text resources and text tool
results.

## Example

Startup instruction:

```
Listen on port 8000 via MCP.
Resources: file:///README.md (project documentation).
Tools: calculate(expression) - evaluate arithmetic.
Prompts: code-review.
Declare all of these on initialize.
```

Handler response to `mcp_tools_call`:

```json
{"actions": [{"type": "mcp_tools_call_response",
              "response": {"content": [{"type": "text", "text": "4"}], "isError": false}}]}
```

Handler response reporting an error:

```json
{"actions": [{"type": "mcp_error_response", "code": -32601, "message": "Tool not found"}]}
```

## Testing

`tests/server/mcp/e2e_test.rs` drives raw JSON-RPC over `reqwest` — **no MCP SDK client is used
anywhere in the repo**, which is why the SSE-transport gap above went unnoticed.

All 9 pass, plus `llm_failure_test.rs`. This file used to say four of them failed, with their
mocks returning `{"type": "send_jsonrpc_response", ...}` — a *jsonrpc* action, not an MCP one.
That was fixed in the test file and the claim here outlived it; re-run before believing any
similar statement.

`tests/client/mcp/e2e_test.rs` also exercises this server, from the other side: NetGet's MCP
client completes the handshake against it and drives `tools/list` → `tools/call` and
`resources/list` → `resources/read`. Those three tests were `#[ignore]`d for lack of mocks
until September 2026, which is how the client came to be sending `initialized` instead of
`notifications/initialized` — this server drops the bare name into
`debug!("Unknown MCP notification")` and a notification has no reply, so nothing failed
visibly on either side.

## References

- [Model Context Protocol](https://modelcontextprotocol.io/)
- [JSON-RPC 2.0](https://www.jsonrpc.org/specification)

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever, and a
hundred of them was a free denial of service on a server that would happily accept a hundred
more. It now declares both halves; the constants and the reasoning live beside them in
`src/server/mcp/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_REQUEST_READ_TIMEOUT` | 30s | MCP rides on HTTP POST and is client-speaks-first: the request line is the first thing on the wire. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 300s | A client holds its connection open between calls — an editor with an MCP server attached may go minutes between tool calls while a human thinks. A reaped connection loses nothing: MCP session state lives in this server's own map, keyed by session id rather than by socket. |
| `MAX_CONNECTIONS` | 256 | Refusal: **HTTP `503` carrying a JSON-RPC error with `MCP_SERVER_BUSY_CODE` (-32000)** — both layers of this protocol's vocabulary at once, and the same code this server already returns when the backend is overloaded. |

**NetGet's own MCP client *speaks inside `connect()`*, so it is never the silent peer this
bound closes:** `src/client/mcp/mod.rs` POSTs `initialize` and then the `initialized`
notification before registering its command channel or calling the model —
`PROTOCOL_QUALITY.md`'s three-state test.

**The deadline covers the read and nothing else.** `axum::serve` owns its accept loop and takes a concrete `TcpListener`, so there is no seam inside it — the same wall `src/server/nfs/guard.rs` hit with `NFSTcpListener`, and the same answer: NetGet keeps the public listener and runs axum behind it on a loopback-only ephemeral port. The relay's deadline re-arms instead of closing while `awaiting_response` is set, which for a strict request/response protocol is exactly "the peer is waiting on us". Two costs are worth stating: `handle_jsonrpc` sees the relay as its peer rather than the real client address, and the loopback backend is reachable by other local processes (again as with NFS). The LLM round-trip, and a `manual`
rule parking an event for a human (`src/state/intercepts.rs`, 300s by default), are outside
every deadline here, so an answer that takes minutes can never close the connection it is an
answer for. That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse:
TFTP evicted live transfers because "idle" was measured wrongly.

`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is removed;
`tests/accept_bounded_test.rs` drives the shared helper, including the guarantee that a busy
connection is never reported as idle.

## No peer handle — `[ message this peer ]` / `[ disconnect this peer ]` stay disabled

This server is a **relay**. NetGet binds the public listener and `axum::serve` runs on a
loopback-only ephemeral port behind it, so the connection the axum handler sees comes from the
relay, not from the client — the cost `mod.rs` already states out loud. A peer handle over the
public half would be a handle over a socket nothing interprets, and one over the backend half
would address the relay.

Even without the relay, `axum::serve` owns its own accept loop and its own framing, and
`McpProtocol`'s actions are `ActionResult::Custom` shaped as JSON-RPC replies to a request that
is in flight. There is no free-standing message to inject.

The dashboard renders that as a dim button reading "this protocol cannot message a peer from
here yet", which is the honest rendering. `tests/peer_handle_coverage_ratchet_test.rs` carries
this protocol on its shrink-only baseline with the reason above, and re-derives the reason from
source on every run, so if the mechanism changes the build fails rather than the file going
quietly stale.
