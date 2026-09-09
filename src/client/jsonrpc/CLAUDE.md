# JSON-RPC 2.0 Client Implementation

## Overview

JSON-RPC 2.0 client over HTTP POST where the LLM controls all RPC method calls, parameters, and can send single
requests, batch requests, or notifications.

## Protocol Version

- **JSON-RPC**: 2.0 (https://www.jsonrpc.org/specification)
- **Transport**: HTTP/1.1 POST with JSON request/response bodies
- **Content-Type**: `application/json`

## Library Choices

### Core Dependencies

- **reqwest** - HTTP client library
    - Chosen for: async/await support, TLS support, ease of use
    - Used for: Making HTTP POST requests to JSON-RPC endpoints
- **serde_json** - JSON serialization/deserialization
    - Chosen for: Standard Rust JSON library
    - Used for: Building JSON-RPC requests and parsing responses
- **tokio** - Async runtime
    - Chosen for: Async HTTP requests

### Why No JSON-RPC Client Library?

- JSON-RPC 2.0 specification is simple (request/response format)
- Direct implementation provides full control over LLM integration
- Most Rust JSON-RPC libraries are server-focused or outdated
- Building on reqwest gives us maximum flexibility

## Architecture Decisions

### Request Types

**Three JSON-RPC Message Types**:

1. **Single Request** - Object with `jsonrpc`, `method`, `params`, `id`
   ```json
   {
     "jsonrpc": "2.0",
     "method": "add",
     "params": [5, 3],
     "id": 1
   }
   ```
    - Expects response with matching `id`

2. **Batch Request** - Array of request objects
   ```json
   [
     {"jsonrpc": "2.0", "method": "add", "params": [1, 2], "id": 1},
     {"jsonrpc": "2.0", "method": "multiply", "params": [3, 4], "id": 2}
   ]
   ```
    - Returns array of responses

3. **Notification** - Request without `id` field
   ```json
   {
     "jsonrpc": "2.0",
     "method": "log_event",
     "params": {"event": "user_action"}
   }
   ```
    - No response expected (fire-and-forget)

### LLM Control Points

**Complete Method Control** - LLM decides:

1. **Method Name**: Which RPC method to call
2. **Parameters**: Method parameters (array or object)
3. **Request ID**: Unique identifier (or omit for notification)
4. **Batch Requests**: Send multiple calls in one request

**Action-Based Requests**:

```json
{
  "type": "send_jsonrpc_request",
  "method": "add",
  "params": [5, 3],
  "id": 1
}
```

Or for batch:

```json
{
  "type": "send_jsonrpc_batch",
  "requests": [
    {"method": "add", "params": [1, 2], "id": 1},
    {"method": "subtract", "params": [10, 5], "id": 2}
  ]
}
```

### Response Handling

**Standard JSON-RPC 2.0 Responses**:

- **Success**: `{"jsonrpc": "2.0", "result": ..., "id": 1}`
- **Error**: `{"jsonrpc": "2.0", "error": {"code": -32601, "message": "Method not found"}, "id": 1}`

LLM receives the full response object and can:

- Parse result values
- Handle error codes and messages
- Make follow-up requests based on responses

### Connection Management

- HTTP-based (connectionless at the JSON-RPC level); each request is independent
- Timeout: 30 seconds per request
- **One `reqwest::Client` per host, built once on the blocking pool and kept.** This was
  wrong in two ways at once: a client was built at connect and dropped on the next line
  (`let _http_client = …`), and `perform_request` / `perform_batch_request` each built a
  fresh one **per request**. `Client::builder().build()` sets up the rustls stack and loads
  the platform root store — on macOS that reads the keychain through Security.framework,
  synchronously and serialised across processes — so on the async runtime it parks a tokio
  worker. Keeping the client also means later requests reuse its connection pool, which is
  why "each request creates a new HTTP connection" is no longer a limitation.
- The cache is keyed by **host**, not process-wide, because `ClientBuilder::resolve` is a
  per-host override and that override is the point: `reqwest` hands the URL host to its DNS
  resolver unconditionally, so `http://127.0.0.1:8080` performs a real `getaddrinfo` — on
  macOS through mDNSResponder, measured blocking for 8.25 s under ~100 concurrent processes.
  A NetGet JSON-RPC client is pointed at a literal IP more often than not.
  `src/client/http/mod.rs` carries the full reasoning; this is a copy of it.

### Chaining: `MAX_FOLLOWUP_DEPTH`

A request the model issues **in answer to a response** gets its own `jsonrpc_response_received`
event, up to a depth of 6. The chain `request → response event → next request` is genuinely
self-referential, so `apply_action` returns a boxed `dyn Future + Send` to cut the type cycle
(an `async fn` awaiting itself is E0391, and `+ Send` has to be named because the deferred arm
awaits it inside a `tokio::spawn`).

This has been wrong twice. First the follow-up actions were discarded outright (`actions: _` at
both notify sites), so answering the response event did nothing — the whole point of raising it.
The repair then dispatched through the `perform_*` cores, which raise no event: bounded, but
exactly **one** step deep, so the second request's own reply went nowhere and the model went
deaf after it. That is the `elasticsearch`/`http2` shape the root CLAUDE.md names, and its
prescribed answer is a depth bound rather than silence.

The bound is checked in `run_follow_ups`, i.e. *after* the event is raised, so the model always
learns what a request answered; only its next request is refused, and the refusal is logged at
WARN and sent to the status stream rather than being silent.
`tests/client/jsonrpc/e2e_test.rs::test_jsonrpc_client_chains_a_second_request_from_a_response`
pins depth 2 and fails if the chain shortens.

### State Management

**Per-Client State**:

```rust
protocol_data: {
  "jsonrpc_client": "initialized",
  "endpoint": "http://localhost:8080",
  "next_id": 1,  // Auto-incrementing request ID (if needed)
  "default_headers": {}  // the `default_headers` startup parameter, when supplied
}
```

**No Session State**:

- Each JSON-RPC call is stateless
- No server-side session management
- LLM maintains conversation context via memory

## LLM Integration

### Events

1. **jsonrpc_connected** - Triggered when client is initialized
    - Parameters: `endpoint` (string)

2. **jsonrpc_response_received** - Triggered when response arrives
    - Parameters:
        - `id` (number | string | null) - Request ID
        - `result` (any) - Result value (if success)
        - `error` (object) - Error object (if error)

### Actions

**Async Actions** (user-triggered):

1. **send_jsonrpc_request** - Send a single JSON-RPC request
    - `method` (string, required)
    - `params` (array | object, optional)
    - `id` (number | string, optional - omit for notification)

2. **send_jsonrpc_batch** - Send multiple requests at once
    - `requests` (array, required)

3. **disconnect** - Close the client

**Sync Actions** (response-triggered):

1. **send_jsonrpc_request** - Make follow-up request based on response

## Limitations

### Not Implemented

- **Transport negotiation** - Only HTTP POST supported (no WebSocket, TCP, etc.)
- **Authentication** - No built-in API key or token handling; the `default_headers` startup
  parameter is the mechanism. It is applied to every single and batch POST by
  `endpoint_and_headers`, under JSON-RPC's mandatory `content-type: application/json` — a
  default can carry `Authorization`, but cannot make the body unparseable to the server.
  Names are lowercased before merging (HTTP header names are case-insensitive) and applied
  from one map, because `reqwest::RequestBuilder::header` appends rather than replaces.
- **Automatic request ID** - LLM must provide IDs (could auto-generate in future)
- **Retry logic** - No automatic retries on failure
- **Chain length** - a response-driven chain stops at `MAX_FOLLOWUP_DEPTH` (6); see above

### Specification Compliance

- **Full JSON-RPC 2.0 compliance** - Follows spec exactly
- **Batch request order** - Response order may not match request order (per spec)
- **Notification handling** - No response parsing for notifications

### LLM Interpretation Challenges

- **ID management** - LLM must track request IDs manually
- **Error handling** - LLM must interpret error codes (-32700, -32600, etc.)
- **Batch complexity** - LLM sees batch as single response array

## Example Prompts and Responses

### Startup

```
open_client jsonrpc http://localhost:8080. This is a JSON-RPC 2.0 calculator service.

Available methods:
- add(a, b): Return sum of a and b
- subtract(a, b): Return a - b
- multiply(a, b): Return a * b
- divide(a, b): Return a / b

Call add(5, 3) first.
```

### Single Request

**LLM Action**:

```json
{
  "type": "send_jsonrpc_request",
  "method": "add",
  "params": [5, 3],
  "id": 1
}
```

**Server Response** (via jsonrpc_response_received event):

```json
{
  "jsonrpc": "2.0",
  "result": 8,
  "id": 1
}
```

**LLM Interpretation**:
"The add method returned 8, which is correct (5 + 3 = 8)."

### Batch Request

**LLM Action**:

```json
{
  "type": "send_jsonrpc_batch",
  "requests": [
    {"method": "add", "params": [1, 2], "id": 1},
    {"method": "multiply", "params": [3, 4], "id": 2},
    {"method": "divide", "params": [10, 2], "id": 3}
  ]
}
```

**Server Response**:

```json
[
  {"jsonrpc": "2.0", "result": 3, "id": 1},
  {"jsonrpc": "2.0", "result": 12, "id": 2},
  {"jsonrpc": "2.0", "result": 5, "id": 3}
]
```

### Error Response

**LLM Action**:

```json
{
  "type": "send_jsonrpc_request",
  "method": "divide",
  "params": [10, 0],
  "id": 4
}
```

**Server Response**:

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32000,
    "message": "Division by zero"
  },
  "id": 4
}
```

**LLM Interpretation**:
"The divide method returned an error: Division by zero (code -32000)."

### Notification (No Response Expected)

**LLM Action**:

```json
{
  "type": "send_jsonrpc_request",
  "method": "log_event",
  "params": {"event": "calculation_started"}
}
```

(Note: No `id` field = notification)

**Server Response**: None (HTTP 204 or empty response)

## References

- [JSON-RPC 2.0 Specification](https://www.jsonrpc.org/specification)
- [reqwest documentation](https://docs.rs/reqwest/)

## Key Design Principles

1. **Simplicity** - Build on HTTP client pattern (reqwest)
2. **LLM Control** - LLM chooses methods, params, and IDs
3. **Spec Compliance** - Strict JSON-RPC 2.0 adherence
4. **Batch Support** - Efficient multi-request handling
5. **Stateless Design** - Each request is independent


## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running JSON-RPC client. The handle is
registered **before** the `jsonrpc_connected` LLM call, because a dashboard-created client
defaults to a `*` → manual rule and that call can park for minutes waiting for a human.

The old "poll `get_client()` every 5 s and exit when it is gone" task is gone: the command
loop is now this client's long-lived task, and it ends when the client is removed
(`remove_client` drops the command sender, so `recv()` returns `None`) or when an injected
`disconnect` arrives. Both the connected-event handler and the command loop go through one
`apply_action`, so the request encoding exists exactly once.

**Outcome semantics — `Executed`, never `Sent`.** reqwest owns the socket and never reports
how many bytes a request serialised to, so a `Sent { bytes_sent }` here would be a number
someone made up. The command loop **awaits** the HTTP round-trip and reports
`Executed { detail: "jsonrpc_request 'add' sent; HTTP 200 (JSON-RPC response received)" }`,
or `... (body was not valid JSON)` when the server answered with something else. A request
that never completes is an `Err`, an unknown action name is `Rejected { error }`, and
`disconnect` is `Disconnected` (the loop ends and the handle is dropped).

`jsonrpc_response_received` still fires, but for an injected request it is raised from its
own registered task (`Dispatch::Deferred`) rather than inline — otherwise a manual rule
parking that LLM call would wedge the command loop for the length of a human's think time
and `send_to_client` would time out on a request that in fact succeeded. `make_request` /
`make_batch_request` are unchanged for callers; each is now `perform_*` (network only)
followed by `notify_*` (the LLM event).
