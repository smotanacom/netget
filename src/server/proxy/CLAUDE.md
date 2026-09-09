# HTTP/HTTPS Proxy Protocol Implementation

## Overview

HTTP/HTTPS proxy server implementing RFC 7230 (HTTP/1.1) with CONNECT method support (RFC 7231) for HTTPS tunneling.
Provides sophisticated Man-in-the-Middle (MITM) capabilities with certificate generation, pass-through mode, and
LLM-controlled filtering.

**Compliance**: HTTP/1.1 (RFC 7230-7235), CONNECT method (RFC 7231 Section 4.3.6)

## Security: this is an unrestricted open relay (read this first)

The destination of every request and every `CONNECT` is chosen by the **peer** —
from the request-target or the `Host` header — and there is no allow-list,
deny-list or network restriction of any kind in this protocol. Loopback,
link-local (including `169.254.169.254`, the cloud instance-metadata endpoint) and
every RFC 1918 range are reachable. Anyone who can reach this port can reach
whatever this host can:

- **SSRF pivot** into the operator's private network.
- **Open relay** someone else's traffic can be laundered through, attributed to
  this machine.

That may be exactly what you want from a honeypot. It is written down here because
nothing in the code refuses it.

**The filter configuration does not change this.** `request_filters`,
`response_filters` and `https_connection_filters` decide what the model is *asked*
about; they restrict nothing on their own. At `FilterMode::None`, or at
`MatchOnly` with filters that miss, the request is forwarded with no decision taken
by anyone. Plain-HTTP *responses* are always forwarded without consultation, and
only the first exchange of a keep-alive HTTPS connection is inspected.

Bounds that do exist: the request head is capped at 64 KiB with a 30s deadline
(408 on expiry), an upstream response is buffered to at most 8 MiB, and CONNECT
tunnels run through `copy_bidirectional` so a half-close propagates. The number of
concurrent connections is unbounded.

## Security: the MITM CA (read this first)

**Where the CA key comes from.** In `certificate_mode: "generate"` a fresh CA key
pair is generated in memory at server start (`ProxyServer::generate_ca_certificate`,
`rcgen::KeyPair::generate`). There is no fixed, hardcoded, or shipped key, and
nothing is read from a well-known path. Every server start produces a different CA.

**Is it persisted?** No. The CA private key exists only in the process's memory and
is dropped when the server stops. Per-domain leaf keys are likewise memory-only.
No intercepted plaintext is written to disk; request/response bodies reach the
`netget.log` tracing sink only at TRACE level, like every other protocol.

**The only disk write** is the optional `ca_export_path` startup parameter, which
writes the CA *certificate* (the public half, safe to distribute). The private key
is never written by any code path and is not reachable through any action.

**What trust the user must grant.** Interception only works against clients that
have been configured to trust that CA — system trust store, `curl --cacert`,
`NODE_EXTRA_CA_CERTS`, browser trust store, etc. Without that grant the client
aborts the handshake with an unknown-issuer error; this proxy cannot silently
intercept an unmodified client. Because the CA is regenerated per run, an exported
and installed certificate stops working after a restart, and stale copies should
be removed from trust stores.

**`certificate_mode: "load_from_file"` is not implemented** and is rejected at
startup. It previously read the key file, *ignored the certificate file entirely*,
and minted a different self-signed CA from the operator's private key — so clients
trusting the real CA rejected the connection while the configuration looked
correct. Supporting a real operator CA requires reading its subject, which needs
rcgen's `x509-parser` feature (not enabled in `Cargo.toml`).

`cert_path` and `key_path` are **declared** in `get_startup_parameters()` even
though the only mode that reads them is rejected. They have to be: `StartupParams`
validates every key against the declared list *before* `add_server`, so while they
were undeclared the caller got `Attempted to access undeclared startup parameter
'cert_path'` instead of the deliberate, explanatory "not implemented" error written
a few lines below the read. Their descriptions say plainly that setting them cannot
make interception use your own CA.

## Startup parameters

`certificate_mode`, `ca_export_path`, `cert_path`, `key_path`,
`request_filter_mode`, `response_filter_mode`, `https_connection_filter_mode` —
seven declared, seven read (`ProxyServer::spawn_with_llm_actions`). None is dead.
Configuration is startup-only; there is no action that changes it on a running
server.

## Library Choices

- **`http-mitm-proxy`** — a real enabled optional dependency of the `proxy` feature
  in `Cargo.toml` that **no file under `src/server/proxy/` references**. It is dead
  weight; removing it is a `Cargo.toml` change.
- **`rcgen`** v0.14 - On-the-fly certificate generation for MITM TLS interception
- **`rustls`** v0.23 + **`tokio-rustls`** v0.26 - TLS stack for MITM (both server and client)
- **`webpki-roots`** v0.26 - Root certificates for validating upstream TLS connections
- **`regex`** - Pattern matching for selective request/response filtering
- **Manual implementation** - Request/response parsing and modification for maximum LLM control
- **`axum`** / **`reqwest`** - Not used; raw TCP/TLS handling for full flexibility

**Why manual implementation**: Provides complete control over every aspect of request/response modification, allowing
the LLM to inject headers, rewrite URLs, modify bodies, and make granular filtering decisions without library
constraints.

## Architecture Decisions

### Three Operating Modes

1. **MITM Mode with Certificate Generation** (`certificate_mode: "generate"`)
    - Generates a self-signed CA certificate at startup
    - Intercepts HTTPS by performing the TLS handshake with the client using a leaf
      minted from that CA
    - Decrypts, inspects and forwards traffic — this is implemented, in `tls_mitm.rs`
    - The CA private key never leaves memory. `ca_export_path` writes the public
      certificate only, and a client that trusts that file has its TLS broken for
      every host, so treat it accordingly.

2. **MITM Mode with Loaded Certificate** (`certificate_mode: "load_from_file"`)
    - **Not implemented, and rejected at startup** with an error naming the
      unimplemented mode. `cert_path` and `key_path` are declared as startup
      parameters only so that rejection is the error the caller sees rather than
      "unknown parameter".
    - Using a real CA needs its subject, which needs rcgen's `x509-parser` feature.
      The previous implementation read the key, ignored the certificate file and
      minted a *different* CA from the operator's private key — interception
      silently failed while appearing configured.

3. **Pass-Through Mode** (certificate_mode = "none")
    - No certificate, no decryption
    - HTTPS CONNECT requests create tunnels without inspection
    - LLM controls allow/block decisions based on destination host/port/SNI only
    - HTTP requests fully inspected and modifiable

### Filter Configuration System

**`ProxyFilterConfig`** allows granular control of LLM involvement:

```rust
pub struct ProxyFilterConfig {
    pub certificate_mode: CertificateMode,               // Generate, LoadFromFile, or None
    pub request_filters: Vec<RequestFilter>,             // host/path/method/header/body regexes
    pub response_filters: Vec<ResponseFilter>,
    pub https_connection_filters: Vec<HttpsConnectionFilter>,
    pub request_filter_mode: FilterMode,
    pub response_filter_mode: FilterMode,
    pub https_connection_filter_mode: FilterMode,
}
```

**FilterMode** (`filter.rs`, `#[serde(rename_all = "lowercase")]`, so the startup
parameters spell these `"all"` / `"match_only"` / `"none"`):

- `All` (the default) — LLM consults on everything
- `MatchOnly` — LLM consults only when a filter matches; with an empty filter list
  this intercepts nothing
- `None` — pass through without the LLM (fast path)

**These select what the model is ASKED about. They restrict nothing.** At `None`,
or at `MatchOnly` with filters that miss, the request goes to whatever destination
the peer named, with no decision taken by anyone. See the security note that opens
`metadata().notes`: this proxy is an unrestricted open relay, loopback and
link-local included.

### Request/Response Lifecycle

**HTTP Request Flow**:

1. Client sends HTTP request → Parse method, URI, headers, body
2. Check `request_filter_mode` + patterns → Decide if LLM consultation needed
3. If consulting LLM → Create `PROXY_HTTP_REQUEST_EVENT` with full request info
4. LLM returns `RequestAction`: Pass, Block, or Modify
5. Apply modifications (headers, path, query params, body, regex replacements)
6. Forward to upstream server
7. Parse response → Check `response_filter_mode`
8. Return response to client (with optional modifications)

**HTTPS CONNECT Flow** (Pass-Through):

1. Client sends `CONNECT host:port` → Parse destination
2. Check `https_connection_filter_mode` + patterns
3. If consulting LLM → Create `PROXY_HTTPS_CONNECT_EVENT` with destination info
4. LLM returns `HttpsConnectionAction`: Allow or Block
5. If allowed → Send `200 Connection Established`, create bidirectional tunnel
6. If blocked → Send `403 Forbidden` with reason

**Access Logging**:

- **DEBUG level**: `[ACCESS] {client_ip} {method} {url} -> {status} {bytes} in {duration}`
- Common Log Format compatible for integration with log analyzers
- Pass-through HTTPS: `[ACCESS] {client_ip} CONNECT {host}:{port} -> TUNNEL {bytes} bytes ({up} up, {down} down) in {duration}`

## LLM Integration

### Action-Based Control

**LLM returns structured actions for granular control**:

**HTTP Request Actions**:

```json
{
  "actions": [
    {
      "type": "handle_request_pass",
      "message": "Allowing request to example.com"
    },
    {
      "type": "handle_request_block",
      "status": 403,
      "body": "Access denied by policy"
    },
    {
      "type": "handle_request_modify",
      "headers": {"X-Proxy": "NetGet"},
      "remove_headers": ["User-Agent"],
      "new_path": "/api/v2",
      "query_params": {"key": "value"},
      "new_body": "modified content",
      "body_replacements": [
        {"pattern": "old", "replacement": "new"}
      ]
    }
  ]
}
```

**HTTPS Connection Actions** (Pass-Through Mode):

```json
{
  "actions": [
    {
      "type": "handle_https_connection_allow",
      "message": "Allowing connection to example.com:443"
    },
    {
      "type": "handle_https_connection_block",
      "reason": "Domain blocked by policy"
    }
  ]
}
```

### Event Types

**`PROXY_HTTP_REQUEST_EVENT`**:

- Triggered: When HTTP request matches filter configuration
- Context: method, url, host, path, headers, body, client_addr
- LLM decides: Pass, Block (status + body), Modify (granular changes)

**`PROXY_HTTPS_CONNECT_EVENT`**:

- Triggered: When HTTPS CONNECT request matches filter configuration
- Context: destination_host, destination_port, sni (from TLS handshake if available), client_addr
- LLM decides: Allow (create tunnel), Block (send 403)

## Connection and State Management

**Per-Connection State**: none. `ProtocolConnectionInfo` is a generic
`serde_json::Value` wrapper (`src/state/server.rs`), not an enum, and the proxy
registers `ProtocolConnectionInfo::empty()` — there is no `Proxy` variant and no
`recent_requests` field anywhere in the tree. Per-connection activity is visible
in the `[ACCESS]` log lines and in the rail's byte counters, which the CONNECT
tunnels feed through `update_connection_stats`.

**Connection Lifecycle**:

1. Accept TCP connection → Add to server connections
2. Read initial HTTP request line
3. If `CONNECT` → Route to `handle_https_connect()`
4. If other method → Route to `handle_http_request()`
5. Process through LLM filtering pipeline
6. Mark connection closed after completion

**Concurrent Connections**: Each connection handled in separate tokio task. No connection limit enforced by default (
production deployments should add rate limiting).

## Certificate Management

**Self-Signed CA Generation**:

```rust
let mut params = CertificateParams::default();
params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
params.distinguished_name.push(DnType::CommonName, "NetGet MITM Proxy CA");
let key_pair = KeyPair::generate()?;
let cert = params.self_signed(&key_pair)?;
```

**Per-Host Certificate Generation** (for MITM):

- Implemented in `cert_cache.rs` using `CertificateCache`
- Generates leaf certificates signed by CA for specific domains
- Caches the leaf certificate **and the key it certifies** per domain (24-hour TTL).
  Caching only the key and regenerating the certificate produces a mismatched pair
  that rustls rejects with `InconsistentKeys(KeyMismatch)`, which broke every
  repeat connection to the same host.
- Supports domain normalization (case-insensitive, trimmed)
- Automatic addition of wildcard and www variants in SAN

**Certificate Installation**:

- Users must install CA certificate in system trust store for MITM mode
- Without installation: the TLS handshake fails with an unknown-issuer error
- Enterprise deployments: Distribute CA via group policy
- Use the `ca_export_path` **startup parameter** to write the CA certificate to a
  file. (There is no `export_ca_certificate` action; the one that used to be
  advertised serialised its arguments and wrote nothing.)
- Installation locations:
  - **macOS**: System Keychain (`/Library/Keychains/System.keychain`)
  - **Windows**: Trusted Root Certification Authorities
  - **Linux**: `/usr/local/share/ca-certificates/` then `update-ca-certificates`

## Implementation Details

### Module Structure

- **`mod.rs`**: Main proxy server, connection handling, pass-through mode
- **`actions.rs`**: LLM action definitions and execution
- **`filter.rs`**: Request/response/HTTPS filtering logic
- **`cert_cache.rs`**: Per-domain certificate caching for MITM
- **`tls_mitm.rs`**: Full TLS MITM orchestration (client accept + upstream connect)

### MITM Flow (tls_mitm.rs)

1. **Send 200 to client**: `HTTP/1.1 200 Connection Established\r\n\r\n`
2. **Generate leaf cert**: `CertificateCache::get_or_generate(domain)` → signed by CA
3. **TLS accept from client**: `TlsAcceptor::accept()` using generated cert
4. **TLS connect to upstream**: `TlsConnector::connect()` with certificate validation
5. **Read HTTP request**: Parse from decrypted client TLS stream
6. **Consult LLM for request**: Create `PROXY_HTTP_REQUEST_EVENT`, get `RequestAction`
7. **Forward to upstream**: Apply request modifications, send via upstream TLS stream
8. **Read HTTP response**: Receive from upstream TLS stream, parse status/headers/body
9. **Consult LLM for response**: Create `PROXY_HTTP_RESPONSE_EVENT`, get `ResponseAction`
10. **Return to client**: Apply response modifications (status, headers, body), send via client TLS stream
11. **Bidirectional copy**: Switch to `tokio::io::copy` for keep-alive connections

### Certificate Cache Design

- **Thread-safe**: `Arc<RwLock<HashMap<String, CachedCert>>>`
- **TTL**: 24 hours, hardcoded. `cert_ttl_secs` is a private field set in
  `CertificateCache::new`; there is no setter and no startup parameter for it.
- **Normalization**: Domains lowercased and trimmed
- **SAN generation**: Automatically adds wildcard (`*.example.com`) and www (`www.example.com`)
- **Automatic cleanup**: Background task runs hourly to remove expired certificates
- **Manual cleanup**: `cleanup_expired()` method available for on-demand maintenance
- **Stats**: `get_stats()` returns `CacheStats` (total, expired, valid)
- **Logging**: Cache stats logged hourly to INFO level

## Current Status

**HTTPS MITM implemented** (`tls_mitm.rs`), with these caveats:

- Self-signed CA generated per run; clients must trust it (see the security
  section above)
- Per-domain leaf certificates cached together with their keys (`cert_cache.rs`)
- Cache cleanup task runs hourly and stops when the server stops
- Client-side TLS accept with the generated certificate; upstream-side TLS
  connect with normal webpki root validation
- Request `pass` / `block` / `modify` and response `pass` / `block` / `modify`
  are all applied on the MITM path
- After the first request/response the connection degrades to an opaque
  bidirectional copy, so only the **first** exchange on a keep-alive connection is
  inspected

**Plain HTTP**: requests are fully inspected and modifiable. **Responses are
not** — `forward_http_request` returns the upstream response to the client
without consulting the LLM, so `proxy_http_response` and the
`handle_response_*` actions only fire on the MITM (HTTPS) path.
`response_filter_mode` likewise only affects MITM.

**Configuration is startup-only.** Certificate mode and the three filter modes are
read once when the server spawns and cloned into each connection; there is no way
to change them on a running server. Six configuration actions used to be
advertised (`configure_certificate`, `configure_request_filters`,
`configure_response_filters`, `configure_https_connection_filters`,
`set_filter_mode`, `export_ca_certificate`) — none of them had any effect, and an
`ActionResult::Output` emitted during a request event was mis-parsed as a
`RequestAction`. They have been removed.

**Testing note**: `cert_cache.rs` has no `#[cfg(test)] mod tests`; the
test-location policy is satisfied here.

## Limitations

1. **HTTP/2 and HTTP/3 Not Supported**
    - Only HTTP/1.1 implemented
    - HTTPS CONNECT tunnels may carry HTTP/2 (transparent to proxy in pass-through)

3. **WebSocket Upgrade Not Handled**
    - WebSocket handshake passes through
    - No LLM inspection of WebSocket frames

4. **Chunked Transfer Encoding**
    - Basic support for reading responses
    - Complex chunked request bodies may not be fully parsed

5. **No Authentication**
    - Proxy doesn't require authentication (anyone can use it)
    - Should add Basic/Digest auth for production

6. **Only the first exchange of an HTTPS keep-alive connection is inspected**
    - After the first request/response the MITM path switches to `tokio::io::copy`

7. **Plain-HTTP responses are not inspected**
    - See Current Status above

### Protocol Compliance Gaps

- Requests are rewritten from absolute-form (`GET http://host/path`) to
  origin-form (`GET /path`) before being forwarded, as RFC 9112 3.2.2 requires,
  and the hop-by-hop `Proxy-Connection` header is dropped. Forwarding the
  absolute-form target verbatim made ordinary origin servers answer 404.
- Missing: Range requests (partial content)
- Missing: 100-Continue handling
- Missing: Upgrade header (WebSocket, HTTP/2)
- Missing: Trailer headers in chunked encoding

## Performance Considerations

**Fast Path**: When `FilterMode::None`, requests pass through without LLM consultation (< 1ms overhead).

**Selective Filtering**: Regex pattern matching on URL/headers adds ~10-100μs overhead.

**LLM Consultation**: Adds 500ms-5s latency per request (depends on model and prompt complexity). Use selective
filtering to minimize impact.

**Concurrent Request Handling**: Each request spawns async task, allowing parallel LLM consultations.

## Example Prompts

### Basic HTTP Proxy

```
Listen on port 8080 using proxy stack. Pass all HTTP requests through unchanged.
```

### Selective Blocking

```
Listen on port 8080 using proxy stack. Block all requests to *.ads.* domains with
status 403 and body "Ads blocked". Pass all other requests through.
```

### Header Injection

```
Listen on port 8080 using proxy stack. For all requests to api.example.com, add
header "Authorization: Bearer TOKEN123" before forwarding.
```

### HTTPS Allow/Block (Pass-Through)

```
Listen on port 8080 using proxy stack with no certificate (pass-through mode).
Block HTTPS connections to facebook.com and twitter.com. Allow all others.
```

### URL Rewriting

```
Listen on port 8080 using proxy stack. Rewrite all requests from /old-api/* to
/new-api/* before forwarding.
```

### Request Body Modification

```
Listen on port 8080 using proxy stack. For POST requests to /api/login, replace
any occurrence of "password" in the body with "hashed_password" before forwarding.
```

### MITM Mode

```
Listen on port 8080 using proxy stack with certificate_mode "generate" and
ca_export_path "./netget-ca.crt". Inspect HTTPS traffic and log any requests
containing credit card patterns.
```

The client must then trust `./netget-ca.crt` (e.g. `curl --cacert ./netget-ca.crt`).

## References

- RFC 7230: HTTP/1.1 Message Syntax and Routing
- RFC 7231: HTTP/1.1 Semantics and Content (CONNECT method)
- RFC 7235: HTTP/1.1 Authentication
- RFC 5246: TLS 1.2 (for HTTPS interception)
- Common Log Format: https://en.wikipedia.org/wiki/Common_Log_Format
- rcgen documentation: https://docs.rs/rcgen/
