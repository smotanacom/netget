# HTTP Client Implementation

## Overview

The HTTP client implementation provides LLM-controlled HTTP/HTTPS requests. The LLM can construct requests with full
control over method, path, headers, and body, and interpret responses.

## Implementation Details

### Library Choice

- **reqwest** - Modern async HTTP client for Rust
- Supports HTTP/1.1, HTTP/2, and HTTPS (TLS via rustls)
- Automatic protocol negotiation via ALPN during TLS handshake
- Timeout handling, redirects, compression
- Note: HTTP/3 support is available but experimental in reqwest, not enabled yet

### Architecture

```
┌──────────────────────────────────────────┐
│  HttpClient::connect_with_llm_actions    │
│  - Store base URL in protocol_data       │
│  - Warm the per-host reqwest client      │
│  - Mark as Connected                     │
└──────────────────────────────────────────┘
         │
         ├─► make_request() - Called per LLM action
         │   - Build request from action data
         │   - Execute via reqwest
         │   - Call LLM with response
         │   - Update memory
         │
         └─► Background Monitor Task
             - Checks if client still exists
             - Exits if client removed
```

### Connection Model

Unlike TCP (persistent connection), HTTP client is **request/response** based:

- "Connection" = initialization of HTTP client
- Each request is independent
- LLM triggers requests via actions
- Responses trigger LLM calls for interpretation

### LLM Control

**Async Actions** (user-triggered):

- `send_http_request` - Make HTTP request
    - Parameters: method, path, headers, body
    - Returns Custom result with request data
- `disconnect` - Stop HTTP client

**Sync Actions** (in response to HTTP responses):

- `send_http_request` - Make follow-up request based on response
- `wait_for_more` - Take no action and wait for the next response

**Events:**

- `http_connected` - Fired when client initialized
- `http_response_received` - Fired when response received
    - Data includes: status_code, status_text, headers, body

### Structured Actions (CRITICAL)

HTTP client uses **structured data**, NOT raw bytes:

```json
// Request action
{
  "type": "send_http_request",
  "method": "GET",
  "path": "/api/users",
  "headers": {
    "Accept": "application/json",
    "Authorization": "Bearer token123"
  },
  "body": null
}

// Response event
{
  "event_type": "http_response_received",
  "data": {
    "status_code": 200,
    "status_text": "OK",
    "headers": {
      "Content-Type": "application/json"
    },
    "body": "{\"users\": [...]}"
  }
}
```

LLMs can construct structured requests and interpret JSON/text responses.

### Request Flow

1. **LLM Action**: `send_http_request` with method, path, headers, body
2. **Action Execution**: Returns `ClientActionResult::Custom` with request data
3. **Request Execution**: `HttpClient::make_request()` called
4. **Response Handling**:
    - Parse status, headers, body
    - Create `http_response_received` event
    - Call LLM for interpretation
5. **LLM Response**: May trigger follow-up requests

### Startup Parameters

- `default_headers` (optional) - Headers included in all requests
    - Example: `{"User-Agent": "NetGet/1.0"}`
    - Applied in `perform_request` **underneath** the headers the model puts on the request
      itself: the two maps are merged on the lowercased header name (HTTP header names are
      case-insensitive) before anything is applied, so `Accept` on the request replaces
      `accept` from the defaults rather than being appended next to it. Merging first is
      what makes that true — `reqwest::RequestBuilder::header` *appends*, so applying both
      sets in turn would put two values on the wire.

### The reqwest client is built once per host, on `spawn_blocking`

`HttpClient::http_client(url)` returns a cached `reqwest::Client`, keyed by host. Three
things depend on that, and all three are recorded in root `CLAUDE.md` as measured:

- `Client::builder().build()` sets up the rustls stack and loads the platform root store.
  On macOS that reads the keychain through Security.framework — synchronous and serialised
  across processes — so it runs on `spawn_blocking`. On the async runtime it parks a tokio
  worker, which is how the `doh` client's whole runtime stalled.
- Keeping the client keeps its connection pool, so requests after the first skip a fresh
  TCP + TLS handshake.
- **The literal-IP resolver bypass.** `reqwest` hands the URL host to its DNS resolver
  unconditionally and `hyper-util`'s `GaiResolver` does not special-case a dotted quad, so
  `http://127.0.0.1:8080` performs a real `getaddrinfo` — measured at **8.25 s** through
  mDNSResponder under ~100 concurrent processes. `ClientBuilder::resolve` is a **per-host**
  override, which is exactly why the cache is keyed by host: one process-wide client cannot
  carry an override for a host it was never told about. This client is pointed at a literal
  IP more often than not, so it is the common case.

`crate::llm::ollama_client::host_of` does the host extraction — the part that is easy to get
wrong, and did get wrong once by leaving the port attached, which silently disabled the
bypass. The override itself is four lines here rather than a call to
`without_dns_for_literal_ip`, which is private and hard-wires reqwest's default TLS backend
(native-tls on macOS: the keychain path the first bullet is about).

### Follow-up depth

`notify_response` is recursive and bounded by `MAX_FOLLOWUP_DEPTH` (4). A model that reads a
response and asks for another request gets the *result* of that request reported back to it,
up to four exchanges deep. It used to stop after one hop — the follow-up ran through a
deliberately non-notifying path — which is the "run through a deliberately non-notifying
path" defect root `CLAUDE.md` lists under known systemic issues. The fix is the one that
file prescribes: box the recursive future with an explicit `+ Send` (it is awaited inside a
`tokio::spawn`) and cap the depth.

### Dual Logging

```rust
info!("HTTP client {} making request: {} {}", client_id, method, url);  // → netget.log
status_tx.send("[CLIENT] HTTP request sent");                          // → TUI
```

### Error Handling

- **Connection Failed**: Initialization error, client not created
- **Request Failed**: Log error, return Err, don't crash client
- **Timeout**: reqwest handles with 30s timeout
- **LLM Error**: Log, continue accepting actions

## Features

### Supported Methods

- GET, POST, PUT, DELETE, PATCH, HEAD

### Supported Features

- ✅ HTTPS (TLS via rustls)
- ✅ HTTP/1.1 and HTTP/2 (automatic protocol negotiation via ALPN)
- ✅ Custom headers
- ✅ Request body (JSON, text, etc.)
- ✅ Response parsing (status, headers, body)
- ✅ Timeouts (30s default)
- ✅ Automatic redirects (reqwest default)

### URL Handling

- Base URL stored in `protocol_data`
- Absolute URLs: `https://example.com/path`
- Relative paths: `/api/users` → `{base_url}/api/users`

## Limitations

- **No Streaming** - Full response buffered in memory, with **no size cap**: a server
  that streams without end grows the buffer until the process dies. (The HTTP/3 client
  bounds this at 8 MiB; this one does not, because `reqwest::Response::text()` gives no
  incremental hook without rewriting the read as a stream.)
- **No File Uploads** - Body is text/JSON only
- **No Cookie Jar** - Each request independent
- **No Custom TLS Config** - rustls is selected explicitly (`use_rustls_tls`), but there
  is no startup parameter for CA roots, client certificates, or verification policy

## Usage Examples

### Simple GET Request

**User**: "Connect to http://httpbin.org and get /status/200"

**LLM Action**:

```json
{
  "type": "send_http_request",
  "method": "GET",
  "path": "/status/200"
}
```

### POST with JSON Body

**User**: "Post user data to /api/users"

**LLM Action**:

```json
{
  "type": "send_http_request",
  "method": "POST",
  "path": "/api/users",
  "headers": {
    "Content-Type": "application/json"
  },
  "body": "{\"name\": \"Alice\", \"email\": \"alice@example.com\"}"
}
```

### Authenticated Request

**User**: "Fetch user profile with auth token"

**LLM Action**:

```json
{
  "type": "send_http_request",
  "method": "GET",
  "path": "/api/me",
  "headers": {
    "Authorization": "Bearer eyJhbGc..."
  }
}
```

## Testing Strategy

See `tests/client/http/CLAUDE.md` for E2E testing approach.

## Future Enhancements

- **Streaming Responses** - For large files
- **Multipart Uploads** - For file uploads
- **Cookie Management** - Persistent sessions
- **WebSocket Upgrade** - For real-time communication
- **Custom TLS Config** - Client certificates, custom CA

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running HTTP client. The handle is
registered **before** the `http_connected` LLM call, because a dashboard-created client
defaults to a `*` → manual rule and that call can park for minutes waiting for a human.

The old "poll `get_client()` every 5 s and exit when it is gone" task is gone. Its removal
check is now one arm of the command loop's `select!`; the other arm drains
`ClientCommand`s. Both the connected-event handler and the command loop go through one
`apply_action`, so the `http_request` decoding exists once.

**Outcome semantics — `Executed`, never `Sent`.** reqwest owns the socket and never reports
how many bytes a request serialised to, so a `Sent { bytes_sent }` here would be a number
someone made up. The command loop **awaits** the exchange and reports
`Executed { detail: "http_request GET /path -> 200 (17 byte body)" }`; a request that never
completes is an `Err`, an unknown action is `Rejected`, and `disconnect` is `Disconnected`
(the loop ends and the handle is dropped).

The `http_response_received` event still fires, but from its own registered task rather
than inline — otherwise a manual rule parking that LLM call would wedge the command loop
for the length of a human's think time and `send_to_client` would time out on a request
that in fact succeeded. `make_request` is unchanged for callers; it is now
`perform_request` (network only) followed by `notify_response` (the LLM event).
