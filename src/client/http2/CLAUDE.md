# HTTP/2 Client Implementation

## Overview

The HTTP/2 client enables LLM-controlled HTTP/2 requests to remote servers. It provides transparent multiplexing, header
compression (HPACK), and server push capabilities.

## Library Choices

### reqwest (v0.12+)

Primary HTTP client library with built-in HTTP/2 support:

**Pros:**

- Automatic HTTP/2 negotiation via ALPN
- `http2_prior_knowledge()` for forcing HTTP/2
- Mature, well-tested library
- Built on top of hyper and h2 crates
- Handles all HTTP/2 complexity (multiplexing, flow control, HPACK)

**Cons:**

- No direct access to server push (limitation of current reqwest API)
- Less control over HTTP/2-specific features

### h2 (underlying crate)

Low-level HTTP/2 implementation used by hyper/reqwest:

**Not used directly** because reqwest provides a simpler API and handles edge cases better.

## Architecture

### Connection Model

Unlike HTTP/1.1, HTTP/2 connections are persistent and multiplexed:

1. **Logical Connection**: HTTP/2 client maintains a single TCP connection for multiple concurrent requests
2. **Stream Multiplexing**: Multiple requests share one connection without head-of-line blocking
3. **Request on Demand**: Requests are made via LLM actions, not a continuous read loop

### Client initialization — once per host, on `spawn_blocking`

`Http2Client::http2_client(url)` returns a cached `reqwest::Client`, keyed by host:

```rust
let mut builder = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(30))
    .http2_prior_knowledge();          // Force HTTP/2 without ALPN negotiation
if let Ok(ip) = host.parse::<std::net::IpAddr>() {
    builder = builder.resolve(&host, std::net::SocketAddr::new(ip, 0));
}
```

**It was built per request**, plus one more at connect that was bound to `_http_client` and
dropped immediately — both on the async runtime. Three things that cost, all recorded in
root `CLAUDE.md` as measured rather than theoretical:

- `Client::builder().build()` sets up the rustls stack and loads the platform root store;
  on macOS that reads the keychain through Security.framework, synchronously and serialised
  across processes. On the async runtime it parks a tokio worker — how the `doh` client's
  whole runtime stalled. It now runs on `spawn_blocking`.
- A fresh client per request means a fresh connection pool per request, so every request
  paid for a new TCP handshake. For a protocol whose entire point is multiplexing over one
  connection, that is the worst possible arrangement.
- **The literal-IP resolver bypass.** `reqwest` hands the URL host to its DNS resolver
  unconditionally and `hyper-util`'s `GaiResolver` does not special-case a dotted quad, so
  `http://127.0.0.1:8080` performs a real `getaddrinfo` — measured at **8.25 s** through
  mDNSResponder under ~100 concurrent processes. `ClientBuilder::resolve` is a **per-host**
  override, which is why the cache is keyed by host rather than being one client.

`crate::llm::ollama_client::host_of` does the host extraction — the part that is easy to
get wrong, and did get wrong once by leaving the port attached, silently disabling the
bypass.

**`http2_prior_knowledge()`**: Forces HTTP/2 protocol without TLS ALPN negotiation. Use this when:

- Server is known to support HTTP/2 over cleartext (h2c)
- Testing HTTP/2-specific features
- ALPN negotiation is not available

For HTTPS with automatic negotiation, omit `http2_prior_knowledge()` and let ALPN handle protocol selection.

### State Management

HTTP/2 client stores minimal state in `protocol_data`:

- `http2_client`: Initialization status
- `base_url`: Base URL for relative requests
- `default_headers`: the `default_headers` startup parameter, when one was supplied

### Startup Parameters

- `default_headers` (optional) — headers included in every request.
  `perform_request` merges them **underneath** the headers the model puts on the request
  itself, keyed by the lowercased header name (HTTP header names are case-insensitive), so
  `Accept` on the request replaces `accept` from the defaults. The merge happens before any
  header is applied because `reqwest::RequestBuilder::header` *appends* — applying both sets
  in turn would put two values of the same header on the wire.

Memory updates from LLM are stored per-client via `AppState::set_memory_for_client()`.

## LLM Integration

### Event Types

1. **`http2_connected`**: Triggered when client is initialized
    - Parameters: `base_url`

2. **`http2_response_received`**: Triggered when HTTP/2 response is received
    - Parameters: `status_code`, `status_text`, `http_version`, `headers`, `body`

### Action Flow

1. User opens HTTP/2 client with instruction
2. Client initializes and enters Connected state
3. LLM receives instruction and available actions
4. LLM generates `send_http2_request` action
5. Client makes HTTP/2 request
6. Response triggers `http2_response_received` event
7. LLM processes response and may generate follow-up actions

### Actions

**Async Actions (user-triggered):**

- `send_http2_request(method, path, headers, body)` - Make HTTP/2 request
- `disconnect()` - Close client

**Sync Actions (response-triggered):**

- `send_http2_request(method, path, headers, body)` - Follow-up request based on response

### Action Execution

```rust
match action_type {
    "send_http2_request" => {
        // Extract parameters
        // Return ClientActionResult::Custom with request data
        // EventHandler processes and calls Http2Client::make_request()
    }
}
```

## HTTP/2 Features

### Multiplexing

Multiple concurrent requests on a single connection:

- Handled automatically by reqwest/h2
- No head-of-line blocking
- Streams are independent

### Header Compression (HPACK)

HTTP/2 compresses headers using HPACK:

- Handled transparently by h2 crate
- Reduces bandwidth for repeated headers
- LLM sees decompressed headers

### Server Push (Limited)

Current reqwest API does not expose server push:

- Server can push resources preemptively
- Pushed resources are accepted but not exposed to application
- Future enhancement: Access pushed resources via h2 directly

### Binary Framing

HTTP/2 uses binary framing:

- Handled by h2 crate
- LLM interacts with text-based API (method, path, headers, body)
- No binary protocol knowledge required

## Limitations

1. **Server Push**: Not exposed by current reqwest API
2. **Stream Priority**: Cannot set stream priorities
3. **Flow Control**: Automatic, cannot tune window sizes
4. **GOAWAY Handling**: Limited control over connection shutdown
5. **Cleartext h2c**: `http2_prior_knowledge()` is applied unconditionally, so this client
   **cannot speak HTTP/2 over TLS**. That is deliberate — NetGet's own HTTP/2 server never
   advertises ALPN, so prior knowledge is the only way to reach it — but it means an
   `https://` target that requires ALPN negotiation will fail. There is no startup
   parameter to switch it off.
6. **No response size cap**: the body is buffered whole via `Response::text()` with no
   limit, then handed to the model

## Testing Strategy

See `tests/client/http2/CLAUDE.md` for full testing documentation.

**Test servers:** none external. Root `CLAUDE.md` forbids tests contacting outside
endpoints; the suite points this client at NetGet's own HTTP/2 server on loopback. The
public servers this section used to list (`http2.golang.org`, `nghttp2.org`) are also all
TLS, which `http2_prior_knowledge()` cannot reach — see Limitations.

**Test Scenarios:**

1. Basic GET request
2. POST with body
3. Custom headers
4. Multiple concurrent requests (multiplexing)
5. Error handling (404, 500)

## Example Prompts

**Basic Request:**

```
Connect to https://http2.golang.org and fetch /reqinfo
```

**POST Request:**

```
Connect to https://httpbin.org and POST to /post with JSON body {"test": "data"}
```

**Multiple Requests:**

```
Connect to https://http2.golang.org, fetch /, then fetch /clockstream
```

## Implementation Notes

### Why `http2_prior_knowledge()`?

Forces HTTP/2 without ALPN negotiation:

- Simplifies testing (no TLS required for h2c)
- Explicit protocol selection
- Useful for HTTP/2-only servers

For production, prefer automatic ALPN negotiation (omit `http2_prior_knowledge()`).

### Request Timeout

30-second timeout prevents hanging on slow servers:

```rust
.timeout(std::time::Duration::from_secs(30))
```

### Memory Management

LLM memory allows stateful interactions:

- Remember previous responses
- Build on prior requests
- Track session data

## Future Enhancements

1. **Server Push Support**: Expose pushed resources via h2 API
2. **Stream Priorities**: Allow LLM to set stream weights
3. **Flow Control Tuning**: Expose window size configuration
4. **GOAWAY Handling**: Better connection lifecycle management
5. **HTTP/2 Upgrade**: Support h2c upgrade from HTTP/1.1

## References

- [RFC 7540: HTTP/2](https://tools.ietf.org/html/rfc7540)
- [reqwest documentation](https://docs.rs/reqwest)
- [h2 crate](https://docs.rs/h2)
- [HTTP/2 Explained](https://http2-explained.haxx.se/)

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running HTTP/2 client. The handle is
registered before `connect()` returns, and the old "poll `get_client()` every 5 s" task is
gone — its removal check is one arm of the command loop's `select!`.

**This is currently the only way anything reaches `Http2Client::make_request`.** The HTTP/2
client raises no connected event: `HTTP2_CLIENT_CONNECTED_EVENT` is declared in `actions.rs`
and never emitted, and nothing else called `make_request`, so before the command channel the
client initialised itself and then did nothing for the rest of its life. Wiring an
`http2_connected` LLM call is the remaining gap.

**Outcome semantics — `Executed`, never `Sent`.** reqwest owns the socket and never reports
how many bytes a request serialised to. The command loop **awaits** the exchange and reports
`Executed { detail: "http2_request GET /path -> 200 (17 byte body)" }`; a failed request is
an `Err`, an unknown action is `Rejected`, `disconnect` is `Disconnected`. The
`http2_response_received` event fires from its own registered task, so a manual rule parking
that LLM call cannot wedge the command loop.

`perform_request` also now prefixes `http://` when the client was opened on a bare
`host:port` (the common case), which reqwest requires; `http2_prior_knowledge()` speaks
cleartext h2c, so that is the correct scheme for it.
