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
│  - Initialize reqwest client             │
│  - Store base URL in protocol_data       │
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

- **No Streaming** - Full response buffered in memory
- **No File Uploads** - Body is text/JSON only
- **No Cookie Jar** - Each request independent
- **No Custom TLS Config** - Uses reqwest defaults
- **No Connection Pooling** - Each request creates new connection

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
