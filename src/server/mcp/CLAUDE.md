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
  backpressure — so a client posting at axum's 2 MiB default limit could enqueue faster than the
  TUI drains.
- **Connections no longer leak.** `initialize` registers a connection for visibility and marks
  it closed on every exit path. Each was previously left `Active` forever, so repeating
  `initialize` grew `AppState` without bound.
- **Protocol version is negotiated**, not hardcoded. The fallback response echoes the client's
  requested revision if it is one of `2024-11-05`, `2025-03-26`, `2025-06-18`, and otherwise
  offers `2024-11-05`. It used to answer `2024-11-05` unconditionally, telling a client on a
  newer revision that its request had been honored.
- Body size is capped only by axum's 2 MiB `DefaultBodyLimit` — a framework default, not a
  deliberate one. `serde_json`'s 128-level recursion limit is what stops deep nesting.
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

**4 of its 9 tests fail, and did so before any of the above changes**
(`test_mcp_initialize`, `test_mcp_resources_list`, `test_mcp_tools_list`,
`test_mcp_prompts_list`). Their mocks return `{"type": "send_jsonrpc_response", ...}`, which is
an action of the *jsonrpc* protocol and not of MCP, so `execute_action` rejects it, the retry
loop exhausts, and the client gets `-32603`. The fix belongs in the test file — the mocks should
return `mcp_initialize_response`, `mcp_resources_list_response`, `mcp_tools_list_response` and
`mcp_prompts_list_response` — and is not made here.

## References

- [Model Context Protocol](https://modelcontextprotocol.io/)
- [JSON-RPC 2.0](https://www.jsonrpc.org/specification)
