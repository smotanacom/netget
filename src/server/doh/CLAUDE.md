# DNS-over-HTTPS (DoH) Protocol Implementation

## Overview

DNS-over-HTTPS server implementing RFC 8484 for secure DNS queries over HTTPS transport. The LLM controls DNS responses
while NetGet handles HTTPS server operations, TLS encryption, and HTTP/2 protocol.

**Status**: Beta (Core Protocol)
**RFC**: RFC 8484 (DNS Queries over HTTPS), RFC 1035 (DNS), RFC 7540 (HTTP/2)
**Port**: 443 (HTTPS, typically), declared as `PrivilegeRequirement::PrivilegedPort(443)`

### What "Beta" covers here

DoH contributes the HTTPS/HTTP2 transport and the RFC 8484 request encodings;
every DNS semantic - the action set, action execution, response construction -
is the DNS protocol's, reached through delegation (see below). Both the GET
(base64url `?dns=`) and POST (`application/dns-message` body) paths are
exercised by `tests/server/doh/e2e_test.rs` against **reqwest** (hyper + rustls under the hood).
Not covered: HTTP/1.1, CA-signed or custom certificates, rate limiting, caching,
EDNS0.

### ALPN

The listener advertises **`h2`**, via
`tls_cert_manager::generate_default_tls_config_with_alpn(&["h2"])` rather than the shared
`generate_default_tls_config()` — `dot`, `tls` and `quic` are built from that default and
document themselves as ALPN-less, so it must stay that way for them.

It advertised nothing until August 2026, and this server speaks only HTTP/2. A client that
negotiates normally therefore had no way to learn the protocol: it fell back to HTTP/1.1 and
hyper's `http2::Builder` rejected the connection with "http2 error", or it refused outright.

**The E2E test could not have caught this**, because `create_insecure_client()` uses
`http2_prior_knowledge()`, which asserts HTTP/2 out of band and skips ALPN entirely. Dropping
that option does not fix the coverage either: under `danger_accept_invalid_certs` reqwest builds
its own rustls `ClientConfig` that does not offer `h2`, so it simply sends HTTP/1.1 and fails.
`server_advertises_h2_alpn` covers the property directly instead, and also asserts the shared
default is still ALPN-less so this cannot be "fixed" by changing it under the other three.

Still unproven end to end: that a client which negotiates via ALPN completes a real DoH query.
That needs a client which offers `h2` while trusting a self-signed certificate.

## Library Choices

- **hickory-proto** (formerly trust-dns) - DNS protocol parsing and construction
    - Reuses DNS wire format handling from standard DNS implementation
    - Parses queries and constructs responses
    - Handles all DNS record types (A, AAAA, MX, TXT, CNAME, etc.)
- **hyper** - HTTP/2 server implementation
    - Async HTTP server with HTTP/2 support
    - Handles request routing and response generation
    - Service-based architecture for request handling
- **tokio-rustls** - Async TLS server implementation
    - Provides TLS acceptor for TCP streams
    - Uses rustls for TLS protocol
    - Self-signed certificates generated automatically
- **base64** - Base64url encoding/decoding
    - Required for GET method (DNS query in URL parameter)
    - URL-safe encoding without padding

**Rationale**: DoH is DNS delivered over HTTP/2 with TLS. By combining hickory-proto (DNS), hyper (HTTP/2), and
tokio-rustls (TLS), we get a complete secure DNS over HTTPS solution. The LLM focuses on DNS semantics while libraries
handle HTTP and encryption.

## Architecture Decisions

### 1. Delegation to DNS Protocol

DoH defines **no actions of its own**. `DohProtocol` wraps a `DnsProtocol` and
forwards everything DNS-shaped to it:

- `get_sync_actions()` / `get_async_actions()` return the DNS action definitions
  verbatim
- `execute_action()` forwards to `DnsProtocol::execute_action`
- `DOH_QUERY_EVENT` is built by calling `DnsProtocol::get_sync_actions()` and
  attaching that list to the event

The only thing DoH owns is the event type (`doh_query`, one event, carrying
extra `peer_addr` and `method` fields) and the transport. So "DoH has zero
actions defined in `src/server/doh/actions.rs`" is true as a grep result and
misleading as a statement about capability: the LLM sees the full DNS action set
on every `doh_query`, and this delegation is what keeps DNS, DoT and DoH
answering identically.

### 2. HTTP/2 Transport Layer

Connection flow:

1. Accept TCP connection on port 443
2. Perform TLS handshake using self-signed certificate
3. Establish HTTP/2 connection over TLS
4. Handle HTTP requests (GET or POST) to `/dns-query` endpoint
5. Extract DNS query from request (URL param or body)
6. Parse DNS query with hickory-proto
7. Send to LLM for response
8. Return DNS response as HTTP body with `application/dns-message` content-type

### 3. Two HTTP Methods Supported

#### GET Method (RFC 8484 Section 4.1)

- DNS query encoded as base64url in `dns=` query parameter
- Example: `GET /dns-query?dns=AAABAAABAAAAAAAAA3d3dwdleGFtcGxlA2NvbQAAAQAB`
- URL-safe base64 encoding (no padding)
- Lightweight, can be cached by HTTP proxies
- Used by web browsers via `<link>` tags

#### POST Method (RFC 8484 Section 4.1)

- DNS query sent as request body
- Content-Type: `application/dns-message`
- Binary DNS packet (not encoded)
- More efficient for large queries
- Used by DoH client libraries

### 4. Self-Signed Certificates

TLS configuration:

- Certificates generated by `tls_cert_manager::generate_default_tls_config()`
- Self-signed for testing/development
- Clients must disable certificate verification for testing
- Production usage requires proper CA-signed certificates

### 5. Dual Logging

- **DEBUG**: Query summary ("DoH query: example.com A via GET"), HTTP method, byte counts
- **TRACE**: Full hex dumps of DNS packets (both request and response)
- Both go to netget.log and TUI Status panel

### 6. HTTP/2 Connection Management

Unlike DoT (raw TLS) and UDP DNS:

- HTTP/2 allows multiplexed requests over single connection
- Connection pooling handled by hyper
- Each request is independent (RESTful API)
- No persistent state between requests

## LLM Integration

### Event Type

**`doh_query`** - Triggered when DNS query received over HTTPS

Event parameters:

- `query_id` (number) - DNS transaction ID from request packet
- `domain` (string) - Domain name being queried
- `query_type` (string) - Record type (A, AAAA, MX, TXT, etc.)
- `peer_addr` (string) - Client IP address and port
- `method` (string) - HTTP method used (GET or POST)

### Available Actions

DoH reuses all DNS actions, and only those:

- `send_dns_a_response` - Return IPv4 address (A record)
- `send_dns_aaaa_response` - Return IPv6 address (AAAA record)
- `send_dns_mx_response` - Return mail exchange record
- `send_dns_txt_response` - Return text record
- `send_dns_cname_response` - Return canonical name alias
- `send_dns_nxdomain` - Domain does not exist
- `send_dns_response` - Hand-assembled response, hex-encoded (escape hatch)
- `ignore_query` - Answered as HTTP 404 rather than by dropping the request,
  since HTTP requires a response

See `src/server/dns/CLAUDE.md` for detailed action documentation.

`dns_response` was previously listed here; no such action exists - the real name
is `send_dns_response`.

As with plain DNS, a **static** event handler cannot serve DoH answers: it
cannot echo the client's random transaction ID or the queried name. Use script
mode for deterministic answers; static mode is only useful for `ignore_query`.

### Example LLM Response

```json
{
  "actions": [
    {
      "type": "send_dns_a_response",
      "query_id": 12345,
      "domain": "example.com",
      "ip": "93.184.216.34",
      "ttl": 300
    },
    {
      "type": "show_message",
      "message": "Resolved example.com to 93.184.216.34 over HTTPS"
    }
  ]
}
```

## Connection Management

### Startup

`DohServer::spawn` binds the `TcpListener` **before** spawning the accept loop
and returns the bound `SocketAddr`. That ordering matters: if the bind were done
inside the spawned task, a port conflict or permission error would be swallowed
by the background task while `open_server` reported the server as `Running`, and
a caller that asked for port 0 would be told the port was 0. The accept loop's
`JoinHandle` is registered with `AppState::register_server_task` so
`stop_server` can abort it and release the port.

### Connection Lifecycle

1. **Accept**: TCP listener accepts connection on port 443
2. **TLS Handshake**: `TlsAcceptor::accept()` performs TLS negotiation
3. **HTTP/2 Setup**: hyper establishes HTTP/2 connection
4. **Request Loop**: Handle multiple HTTP requests over same connection. DoH
   adds no entry to `ServerInstance.connections`, so DoH connections are
   invisible to the TUI connection list and to per-connection scheduled tasks
5. **Close**: Connection ends when client closes or error occurs

### State Management

- DoH connections are stateless at application level (REST API)
- HTTP/2 manages connection multiplexing
- Each request is independent (no state between requests)
- No connection state stored in ServerInstance

## Known Limitations

### 1. Self-Signed Certificates Only

- No support for custom certificates
- No CA-signed certificate loading
- No certificate rotation/renewal
- Testing requires disabling certificate verification on clients

**Workaround**: For production, modify `tls_cert_manager` to load custom certificates.

### 2. No Path Routing At All

The request path is never examined. `/dns-query` is the conventional RFC 8484
endpoint and is what clients will use, but any path - `/`, `/foo` - is served
identically, and there is no way to configure or reject one. This is lenient
rather than broken (RFC 8484 treats the path as discovered from a URI template,
not fixed), but it does mean the server cannot host anything else alongside DoH.

### 3. No HTTP/1.1 Support

- Only HTTP/2 supported
- HTTP/1.1 DoH clients will fail
- Fallback not implemented

**Reason**: RFC 8484 recommends HTTP/2 for performance (multiplexing).

### 4. DNS Limitations Inherited

All limitations from standard DNS protocol apply:

- Single answer per action (use `send_dns_response` for multiple)
- Limited record type support
- No DNSSEC
- No recursive resolution
- See `src/server/dns/CLAUDE.md` for full list

### 5. No Rate Limiting

- No per-client request limits
- No connection limits
- Vulnerable to DoS attacks without external rate limiting

### 6. No Caching

- No HTTP cache headers
- No query result caching
- Each request hits LLM or script

**Future Enhancement**: Add cache-control headers based on DNS TTL.

## Example Prompts

### Basic DoH Server

```
listen on port 443 via dns-over-https
Respond to all A record queries for example.com with IP 93.184.216.34
For all other domains, return NXDOMAIN
```

### Multi-Method DoH Server

```
listen on port 443 via doh
Support both GET and POST methods for DNS queries
For secure.example.com:
  - A record: 93.184.216.34
  - AAAA record: 2001:db8::1
For unknown domains, return NXDOMAIN
```

### Privacy-Focused DoH

```
listen on port 443 via dns-over-https
Provide DNS resolution over HTTPS for privacy
Block tracking domains by returning NXDOMAIN for:
  - doubleclick.net
  - google-analytics.com
For legitimate domains, resolve normally
```

## Performance Characteristics

### Latency

- **TLS Handshake**: ~50-100ms (one-time per connection)
- **HTTP/2 Setup**: ~10ms (one-time per connection)
- **Per Query (with scripting)**: 1-2ms (after connection setup)
- **Per Query (without scripting)**: 2-5 seconds (LLM call)
- hickory-proto parsing: ~10-50 microseconds
- Base64 decode (GET): ~1-5 microseconds

### Throughput

- **With Scripting**: Thousands of queries per second
- **Without Scripting**: ~0.2-0.5 QPS (LLM-limited)
- HTTP/2 multiplexing enables concurrent requests
- Connection reuse amortizes TLS/HTTP/2 setup

### Method Comparison

- **GET**: Slightly slower (base64 decode), but cacheable by proxies
- **POST**: Slightly faster (no encoding), better for large queries
- Performance difference negligible in practice

### Comparison to DoT and UDP DNS

- **vs DoT**: Similar performance, but HTTP/2 multiplexing better for concurrent queries
- **vs UDP DNS**: Higher latency (TLS+HTTP overhead), but encrypted and tamper-proof
- **Use Case**: Web browsers, privacy-focused DNS clients, corporate environments

### Scripting Compatibility

Excellent scripting candidate:

- Same query/response pattern as DNS/DoT
- Deterministic responses based on domain
- HTTP/2 makes scripting very efficient
- High query volume typical

## Security Considerations

### TLS + HTTP/2 Protection

- Prevents eavesdropping on DNS queries
- Prevents DNS spoofing/tampering
- Ensures query integrity
- Authenticates server (with proper certificates)
- HTTP/2 encryption provides additional layer

### Self-Signed Certificate Risks

- Default setup vulnerable to MITM attacks
- Clients must explicitly trust self-signed cert
- Not suitable for production without proper PKI

### Privacy Benefits

- ISP/network operators cannot see DNS queries
- Prevents DNS-based tracking
- Protects against DNS hijacking
- Indistinguishable from HTTPS traffic (bypasses censorship)
- Complements HTTPS for end-to-end privacy

### HTTP GET Method Considerations

- GET requests can be logged by proxies
- DNS query visible in proxy logs (base64 encoded)
- POST method preferred for sensitive queries

## References

- [RFC 8484: DNS Queries over HTTPS (DoH)](https://datatracker.ietf.org/doc/html/rfc8484)
- [RFC 7540: Hypertext Transfer Protocol Version 2 (HTTP/2)](https://datatracker.ietf.org/doc/html/rfc7540)
- [RFC 1035: Domain Names - Implementation](https://datatracker.ietf.org/doc/html/rfc1035)
- [hickory-proto Documentation](https://docs.rs/hickory-proto/latest/hickory_proto/)
- [hyper Documentation](https://docs.rs/hyper/latest/hyper/)
- [tokio-rustls Documentation](https://docs.rs/tokio-rustls/latest/tokio_rustls/)
- [Mozilla DoH Documentation](https://wiki.mozilla.org/Trusted_Recursive_Resolver)

---

## `test_doh_server` and machine load — an open item, deliberately not "fixed" (August 2026)

`test_doh_server` times out intermittently in a full `--test-threads=100` run. The failure is
always the same: `reqwest ... source: TimedOut` on the first GET, against a server that logged
`DoH server listening on 127.0.0.1:<port>` and never received a request — the mock reports zero
`doh_query` calls.

**What was ruled out, so it is not re-investigated from scratch:**

- *A port mismatch.* The harness resolves the real bound port correctly (the mock answers the
  startup call with `port: 0`); the failing run queried the port the server was listening on.
- *The `has_packet_capture_access` probe change.* The failure reproduces with both the old
  `/dev/bpf0..3` scan and the current one.
- *`reqwest`'s platform root store.* `tls_built_in_root_certs(false)` was added — it is correct
  regardless, since `danger_accept_invalid_certs` means nothing is checked against those roots —
  and it measurably did **not** change the failure rate.
- *A single-threaded test runtime starving the TLS/HTTP-2 client.* Switching to
  `flavor = "multi_thread"` did not change the failure rate either, and was reverted rather than
  left in as an unproven fix.

**What the evidence actually points at:** external machine load. Three consecutive clean
full-suite runs were recorded on a quiet machine; the failures appeared at a **load average of
93 on 12 cores** with a video call, a VM and WindowServer occupying the box. The three
`bluetooth_ble::read_default_value_test` cases — also wall-clock-deadline tests — start failing
in the same runs, which is the tell: it is not DoH-specific.

The honest reading is that this test asserts a **10-second wall-clock budget** for a loopback
HTTP/2-over-TLS round trip, and that budget is not met when the machine is 8x oversubscribed.
Raising the timeout would only move the threshold, so it was not done. **Do not label this
flaky and move on, and do not "fix" it with a bigger number**: if it reproduces on an idle
machine, there is a real defect here and the ruled-out list above is where to start.
