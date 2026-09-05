# HTTP/3 Client Implementation

## Overview

The HTTP/3 client implementation provides LLM-controlled HTTP/3 requests over QUIC transport. The LLM can construct
requests with full control over method, path, headers, body, and stream priorities, while benefiting from QUIC's
features like multiplexing and 0-RTT connection resumption.

## Implementation Details

### Library Choices

- **quinn** (v0.11) - Async QUIC implementation
    - Modern, pure-Rust QUIC implementation
    - Full HTTP/3 support via QUIC transport
    - TLS 1.3 built-in

- **h3** (v0.0.6) - HTTP/3 implementation
    - RFC 9114 compliant HTTP/3
    - Stream management and frame handling
    - Works on top of any QUIC implementation

- **h3-quinn** (v0.0.7) - Adapter connecting h3 and quinn
    - Bridges h3's trait requirements with quinn's API
    - Provides `h3::quic::Connection` implementation

- **rustls** - TLS implementation for QUIC
    - Certificate verification (currently disabled for testing)
    - Configurable certificate validation

### Architecture

```
┌──────────────────────────────────────────┐
│  Http3Client::connect_with_llm_actions   │
│  - Store base URL and connection info    │
│  - Mark as Connected (logical)           │
│  - Spawn monitoring task                 │
└──────────────────────────────────────────┘
         │
         ├─► make_request() - Called per LLM action
         │   - Create QUIC endpoint
         │   - Configure TLS
         │   - Establish QUIC connection
         │   - Create H3 session
         │   - Build and send HTTP/3 request
         │   - Receive and parse response
         │   - Call LLM with response event
         │   - Close connection
         │
         └─► Background Monitor Task
             - Checks if client still exists
             - Exits if client removed
```

### Connection Model

Unlike TCP (persistent) or HTTP/1.1 (request/response), HTTP/3 uses **QUIC connection per request batch**:

- "Connection" = initialization and readiness
- Each LLM action creates a new QUIC connection
- QUIC connections support multiplexing (multiple streams)
- Future optimization: persistent QUIC connection pool

### QUIC vs TCP Differences

| Aspect                    | TCP (HTTP/1.1)       | QUIC (HTTP/3)        |
|---------------------------|----------------------|----------------------|
| **Transport**             | Stream-based (TCP)   | Datagram-based (UDP) |
| **Encryption**            | Optional (HTTPS)     | Built-in (TLS 1.3)   |
| **Handshake**             | 3-way + TLS (4 RTTs) | Combined (1-2 RTTs)  |
| **0-RTT**                 | Not available        | Supported (0 RTT)    |
| **Multiplexing**          | HTTP/2 only          | Native (streams)     |
| **Head-of-line blocking** | Yes                  | No (per-stream)      |
| **Connection migration**  | Not supported        | Supported            |

### LLM Control

**Async Actions** (user-triggered):

- `send_http3_request` - Make HTTP/3 request
    - Parameters: method, path, headers, body, priority
    - Returns Custom result with request data
- `disconnect` - Stop HTTP/3 client

**Sync Actions** (in response to HTTP/3 responses):

- `send_http3_request` - Make follow-up request based on response

**Events:**

- `http3_connected` - Fired when client initialized
    - Data includes: base_url, connection_id
- `http3_response_received` - Fired when response received
    - Data includes: status_code, status_text, headers, body, stream_id

### Structured Actions (CRITICAL)

HTTP/3 client uses **structured data**, NOT raw bytes:

```json
// Request action
{
  "type": "send_http3_request",
  "method": "GET",
  "path": "/api/users",
  "headers": {
    "Accept": "application/json",
    "Authorization": "Bearer token123"
  },
  "body": null,
  "priority": 5
}

// Response event
{
  "event_type": "http3_response_received",
  "data": {
    "status_code": 200,
    "status_text": "OK",
    "headers": {
      "Content-Type": "application/json"
    },
    "body": "{\"users\": [...]}",
    "stream_id": 0
  }
}
```

LLMs can construct structured requests and interpret JSON/text responses.

### Request Flow

1. **LLM Action**: `send_http3_request` with method, path, headers, body, priority
2. **Action Execution**: Returns `ClientActionResult::Custom` with request data
3. **QUIC Connection**:
    - Create quinn endpoint
    - Configure TLS (with certificate verification)
    - Establish QUIC connection to remote server
4. **HTTP/3 Session**:
    - Create h3 connection over quinn
    - Get SendRequest handle
5. **Request Execution**: `Http3Client::make_request()` called
    - Build HTTP request with headers and body
    - Send request on new stream
    - Send body data if present
    - Finish sending
6. **Response Handling**:
    - Receive response headers
    - Read response body chunks
    - Parse status, headers, body
    - Create `http3_response_received` event
    - Call LLM for interpretation
7. **Cleanup**:
    - Shutdown H3 connection gracefully
    - Close quinn endpoint
8. **LLM Response**: May trigger follow-up requests

### Stream Priorities

HTTP/3 supports stream priorities (0-7, higher = more urgent):

- LLM can specify priority per request
- Allows control over resource loading order
- Currently passed but not fully utilized (quinn handles scheduling)

### Startup Parameters

- `default_headers` (optional) - Headers included in all requests
    - Example: `{"User-Agent": "NetGet-HTTP3/1.0"}`
    - `perform_request` merges them **underneath** the headers the model puts on the request
      itself, keyed by the lowercased header name, so a request header replaces a default of
      the same name. The merge happens before any header is applied, because
      `http::request::Builder::header` *appends* — applying both sets in turn would send the
      same header twice.

`enable_0rtt` **is not a parameter**, and was removed rather than left declared. This client
opens a fresh `quinn::Endpoint` for each request and closes it before returning, and keeps no
session-ticket cache; 0-RTT is purely session *resumption*, so there is never a previous
session to resume from. The knob could not have changed a single request whatever it was set
to. Restoring it means building the connection pool listed under Future Enhancements first.

### Dual Logging

```rust
info!("HTTP/3 client {} making request: {} {}", client_id, method, url);  // → netget.log
status_tx.send("[CLIENT] HTTP/3 request sent");                           // → TUI
```

### Error Handling

- **Connection Failed**: Initialization error, client not created
- **QUIC Connection Failed**: Network unreachable, server down
- **TLS Handshake Failed**: Certificate issues (currently bypassed)
- **HTTP/3 Negotiation Failed**: Server doesn't support HTTP/3
- **Request Failed**: Log error, return Err, don't crash client
- **Timeout**: quinn handles with connection timeout
- **LLM Error**: Log, continue accepting actions

## Features

### Supported Methods

- GET, POST, PUT, DELETE, PATCH, HEAD

### Supported Features

- ✅ QUIC transport (UDP-based)
- ✅ TLS 1.3 encryption (built-in)
- ✅ Stream multiplexing
- ✅ Custom headers
- ✅ Request body (JSON, text, etc.)
- ✅ Response parsing (status, headers, body)
- ✅ Stream priorities (LLM-controlled)
- ✅ Certificate verification bypass (testing mode)

### URL Handling

- Base URL stored in `protocol_data`
- Absolute URLs: `https://example.com/path`
- Relative paths: `/api/users` → `{base_url}/api/users`

## Limitations

- **No Connection Pooling** - Each request creates new QUIC connection
    - Future optimization: persistent connection pool
    - Current approach simpler for LLM control

- **No 0-RTT** - Not implemented despite QUIC support
    - Requires session ticket storage, which requires connection reuse
    - There is no startup parameter for it: see Startup Parameters

- **No Streaming** - Full response buffered in memory
    - Same limitation as HTTP/1.1 client

- **No File Uploads** - Body is text/JSON only

- **Certificate Verification Disabled** - For testing
    - Should be configurable via startup params
    - Currently uses `SkipServerVerification`

- **No Connection Migration** - Client doesn't handle IP changes
    - QUIC supports this but not implemented

- **Per-Request Connection** - Connection not reused
    - Negates some QUIC benefits (0-RTT, connection warmup)
    - Trade-off for simpler LLM control

## Usage Examples

### Simple GET Request

**User**: "Connect to https://cloudflare-quic.com:443 and get /cdn-cgi/trace using HTTP/3"

**LLM Action**:

```json
{
  "type": "send_http3_request",
  "method": "GET",
  "path": "/cdn-cgi/trace"
}
```

### POST with JSON Body and Priority

**User**: "Post user data to /api/users with high priority"

**LLM Action**:

```json
{
  "type": "send_http3_request",
  "method": "POST",
  "path": "/api/users",
  "headers": {
    "Content-Type": "application/json"
  },
  "body": "{\"name\": \"Alice\", \"email\": \"alice@example.com\"}",
  "priority": 7
}
```

### Authenticated Request

**User**: "Fetch user profile with auth token, prioritize this request"

**LLM Action**:

```json
{
  "type": "send_http3_request",
  "method": "GET",
  "path": "/api/me",
  "headers": {
    "Authorization": "Bearer eyJhbGc..."
  },
  "priority": 6
}
```

## Testing Strategy

See `tests/client/http3/CLAUDE.md` for E2E testing approach.

### Test Servers

- **Cloudflare QUIC**: https://cloudflare-quic.com
- **Google**: https://quic.nginx.org (if available)
- **Local**: nginx with HTTP/3 support

## Implementation Challenges

### 1. QUIC Ecosystem Maturity

- h3 crate is v0.0.x (early stage)
- API may change between versions
- Limited documentation

### 2. Certificate Verification

- Currently disabled for testing
- Should be configurable
- LLM should control verification policy

### 3. Connection Lifecycle

- Per-request connections simpler but inefficient
- Connection pooling would be better
- Trade-off: simplicity vs performance

### 4. Stream ID Access

- h3/quinn don't expose stream IDs easily
- Using placeholder `0` for now
- May need to track manually

### 5. 0-RTT Implementation

- Requires session ticket storage
- Needs persistent state between requests
- Conflicts with per-request connection model

## Future Enhancements

### High Priority

- **Connection Pooling** - Reuse QUIC connections
    - Dramatically improves performance
    - Enables 0-RTT on subsequent requests
    - Requires connection lifecycle management

- **Configurable Certificate Verification**
    - Startup param for verification policy
    - Support for custom CA certificates
    - LLM-controlled decision

### Medium Priority

- **0-RTT Support** - Faster connection resumption
    - Store session tickets
    - LLM decides when to use 0-RTT

- **Stream Multiplexing** - Concurrent requests on one connection
    - Send multiple requests simultaneously
    - Better utilize QUIC benefits

- **WebSocket over HTTP/3** - For real-time communication
    - RFC 9220 (WebTransport)
    - Future protocol enhancement

### Low Priority

- **Connection Migration** - Handle IP changes
    - Useful for mobile scenarios
    - Requires connection tracking

- **QUIC Datagrams** - For unreliable data
    - RFC 9221 extension
    - Gaming, VoIP use cases

## Comparison with HTTP/1.1 Client

| Feature          | HTTP/1.1 Client | HTTP/3 Client |
|------------------|-----------------|---------------|
| **Transport**    | TCP             | QUIC (UDP)    |
| **Library**      | reqwest         | quinn + h3    |
| **TLS**          | Optional        | Built-in      |
| **Multiplexing** | No              | Yes (streams) |
| **Priorities**   | No              | Yes (0-7)     |
| **Connection**   | Per request     | Per request*  |
| **0-RTT**        | No              | Not yet**     |
| **Complexity**   | Low             | Medium        |

\* Should be connection pool in future
\*\* Planned enhancement

## References

- **QUIC RFC**: RFC 9000 (QUIC: A UDP-Based Multiplexed and Secure Transport)
- **HTTP/3 RFC**: RFC 9114 (HTTP/3)
- **quinn**: https://github.com/quinn-rs/quinn
- **h3**: https://github.com/hyperium/h3
- **Cloudflare HTTP/3**: https://blog.cloudflare.com/http-3-vs-http-2/

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running HTTP/3 client. The handle is
registered before `connect()` returns, and the old "poll `get_client()` every 5 s" task is
gone — its removal check is one arm of the command loop's `select!`.

**This is currently the only way anything reaches `Http3Client::make_request`.** The HTTP/3
client raises no connected event: `HTTP3_CLIENT_CONNECTED_EVENT` is declared in `actions.rs`
and never emitted, and nothing else called `make_request`, so before the command channel the
client recorded its target and then did nothing. Wiring an `http3_connected` LLM call is the
remaining gap — and note that `tests/client/http3/e2e_test.rs` mocks `http3_connected` with
`expect_calls(1)`, so those tests cannot pass until it is.

**Outcome semantics — `Executed`, never `Sent`.** h3/quinn own the datagrams and report no
wire byte count for the request. The command loop **awaits** the QUIC exchange and reports
`Executed { detail: "http3_request GET /path -> 200 (17 byte body)" }`; a failed request is
an `Err`, an unknown action is `Rejected`, `disconnect` is `Disconnected`. The
`http3_response_received` event fires from its own registered task, so a manual rule parking
that LLM call cannot wedge the command loop.

The QUIC connection is now closed *before* the LLM call rather than after it, so a slow model
no longer holds an idle connection open.
