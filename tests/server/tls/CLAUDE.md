# TLS Protocol Testing

## Overview

E2E tests for the TLS protocol implementation. Tests validate encrypted communication with custom application protocols
running over TLS.

## Test Strategy

Black-box testing approach:

- LLM interprets test prompts
- Real TLS client (tokio-rustls) connects
- Validates TLS handshake and application protocol
- Tests multiple connection patterns

## Test Files

- `e2e_test.rs` - End-to-end tests with real TLS clients
- `llm_failure_test.rs` - what the peer gets when the backend fails: a close_notify alert,
  then `Ok(0)` rather than `UnexpectedEof`
- `queue_limit_test.rs` - the cap on application data queued while an answer is in flight.
  Both directions: over `MAX_QUEUED_BYTES` behind a **parked** record (a `manual` handler,
  which is the 300-second window the cap exists for) must be closed with a close_notify and
  logged `decision=fail_closed_queued_data_overflow`; well under it must **not** be closed,
  without which a server that hung up on everybody would pass the first half. **0** model
  calls in both, by construction — a manual rule answers the event, and a non-zero
  `tls_data_received` count would mean the routing itself had broken.
- `peer_inject_test.rs` - the dashboard's `[ message this peer ]` / `[ disconnect this peer ]`
  path, **0 LLM calls**. In-process, `instruction: Some(String::new())` and a `*` static
  handler, so the model is never reached. It asserts the handle exists before the peer has
  sent an application record (the parked-first-record window), that an injected
  `send_tls_data` comes out of a real rustls client **as plaintext** — the point being that
  the injected bytes go through the same rustls write half the session uses — and that
  `close_connection` produces a clean `Ok(0)`.

## LLM Call Budget

**Target**: < 10 LLM calls per test suite

### Current Tests

1. **`test_tls_echo_server`** - 1 LLM call
    - Single server start with echo protocol
    - 3 connections reuse same server
    - Tests: welcome message, echo functionality

2. **`test_tls_http_like_server`** - 1 LLM call
    - Single server start with HTTP-like protocol
    - 3 requests reuse same server
    - Tests: GET /, GET /api, 404 handling

3. **`queue_limit_test`** (2 tests) - 1 LLM call each (server startup only)

**Total LLM calls**: 4 (well under budget)

## Runtime Characteristics

- **Per test**: ~10-15 seconds (including LLM + TLS handshakes)
- **Full suite**: ~20-30 seconds
- **TLS handshake overhead**: ~50-100ms per connection
- **LLM call**: ~5-10 seconds (one-time per server)

## Test Implementation

### Test 1: TLS Echo Server

Tests basic encrypted echo protocol:

```
Prompt: "listen on port {AVAILABLE_PORT} via tls. When client connects, send 'Welcome to secure echo server\n'. Echo back any received data."

Expected behavior:
- Accept TLS connections
- Send welcome message on connect
- Echo back any received data

Validations:
✓ TLS handshake succeeds
✓ Welcome message received
✓ Data is echoed back correctly
✓ Multiple connections work
```

### Test 2: TLS HTTP-like Server

Tests custom HTTP implementation over TLS:

```
Prompt: "listen on port {AVAILABLE_PORT} via tls. Implement a simple HTTP server:
- For GET /: return 'HTTP/1.1 200 OK\r\n\r\nWelcome'
- For GET /api: return 'HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ok\"}'
- For anything else: return 'HTTP/1.1 404 Not Found\r\n\r\n'"

Expected behavior:
- Accept TLS connections
- Parse HTTP-like requests
- Route to appropriate handler
- Return proper responses

Validations:
✓ TLS handshake succeeds
✓ GET / returns 200 OK with "Welcome"
✓ GET /api returns JSON response
✓ Unknown paths return 404
```

## Client Implementation

### TLS Client Setup

Uses tokio-rustls with self-signed certificate acceptance:

```rust
// Initialize crypto provider
rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

// Create config with no certificate verification
let root_store = RootCertStore::empty();
let mut config = ClientConfig::builder()
    .with_root_certificates(root_store)
    .with_no_client_auth();

config.dangerous().set_certificate_verifier(Arc::new(NoCertificateVerification));

// Connect
let tls_stream = connector.connect(domain_name, tcp_stream).await?;
```

### No Certificate Verification

Custom verifier accepts all certificates (testing only):

- Implements `ServerCertVerifier` trait
- Returns success for all verify operations
- Supports RSA, ECDSA, Ed25519 signatures
- **WARNING**: Never use in production

### Data Exchange Pattern

```rust
// Send data
tls_stream.write_all(data.as_bytes()).await?;
tls_stream.flush().await?;

// Receive response
let mut buffer = vec![0u8; 4096];
let n = tokio::time::timeout(
    Duration::from_secs(5),
    tls_stream.read(&mut buffer)
).await??;
```

## Known Issues

### 1. Self-Signed Certificate Warnings

**Issue**: Clients must disable certificate verification
**Impact**: Low (testing only)
**Workaround**: Use `NoCertificateVerification` verifier

### 2. Connection Reuse

**Issue**: Tests create new connection per exchange
**Impact**: Increases test runtime (TLS handshake overhead)
**Future**: Could pool connections for better performance

### 3. Buffer Size

**Issue**: Fixed 4096-byte buffer may truncate large responses
**Impact**: Low (test responses are small)
**Workaround**: Increase buffer size if needed

## Test Execution

### Run TLS tests only

```bash
./cargo-isolated.sh test --no-default-features --features tls --test tls::e2e_test
```

### With logging

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features tls --test tls::e2e_test -- --nocapture
```

### Prerequisites

- Ollama running locally
- `qwen3-coder:30b` model (or configured default)
- Available ports for testing

## Efficiency Notes

### LLM Call Minimization

- **Single server per test**: All validations against one instance
- **Script mode**: LLM generates script, server reuses it
- **No reconnection overhead**: Server stays alive

### Connection Efficiency

- TLS handshake (~50-100ms) is main overhead
- Could reuse connections within test
- Multiple tests share same binary (fast startup)

### Future Optimizations

1. Connection pooling within tests
2. Parallel test execution
3. Pre-warmed server instances

## Debugging

### Enable trace logging

```bash
RUST_LOG=trace ./cargo-isolated.sh test --features tls --test tls::e2e_test -- --nocapture
```

### Check TLS handshake

Look for:

- "TLS handshake complete with X.X.X.X"
- Certificate generation logs
- Connection state transitions

### Validate protocol data

- DEBUG logs show data summaries
- TRACE logs show full hex/text payloads
- Both sent and received data logged

## Dependencies

- `tokio-rustls` - TLS client for testing
- `rustls` - TLS library
- `tokio` - Async runtime

## Test Coverage

### Covered Scenarios

✓ Basic TLS handshake
✓ Self-signed certificates
✓ Application data exchange
✓ Multiple connections to same server
✓ Text-based protocols
✓ HTTP-like request/response
✓ Error responses (404)

### Not Covered

✗ Certificate validation (disabled in tests)
✗ Client certificates (mutual TLS)
✗ Binary protocols (hex encoding needed)
✗ Large data transfers (> 4KB)
✗ Connection timeouts
✗ Concurrent connections stress test
✗ The `record_overflow` alert itself — rustls will not send an arbitrary fatal alert, so the
  queued-data refusal is a close_notify and the reason lives in the log

## Performance Expectations

### Test Suite

- **2 tests total**
- **~20-30 seconds runtime**
- **2 LLM calls total**
- **~6 TLS connections total**

### Per Test

- **1 LLM call** (script generation)
- **3 client connections** (validation scenarios)
- **~10-15 seconds** (LLM + handshakes + exchanges)

### Bottlenecks

1. LLM call (~5-10s) - largest single delay
2. TLS handshakes (~50-100ms each)
3. Server startup (~1-2s)

## References

- [RFC 8446: TLS 1.3](https://datatracker.ietf.org/doc/html/rfc8446)
- [tokio-rustls Documentation](https://docs.rs/tokio-rustls/latest/tokio_rustls/)
- [rustls Documentation](https://docs.rs/rustls/latest/rustls/)

## `connection_bounds_test.rs` — 4 in-process tests, **0 LLM calls** (the backend is a dead port)

The read deadlines in `src/server/tls/mod.rs` — `HANDSHAKE_READ_TIMEOUT` (60s), `FIRST_RECORD_READ_TIMEOUT` (300s) and `IDLE_AFTER_DATA_TIMEOUT` (300s) — driven from the wire. No
mock: these assert on *clocks*, not on answers, and a reachable backend would only add noise.
Loopback only.

**What each test is for, and why there are three bounds rather than two.** One constant used to
govern both the handshake wait and the wait for the first application record. They face
different peers — one that has not proved it speaks TLS, and one that has — so the tests drive
them separately: a bare `TcpStream` that never sends a ClientHello is closed on
`handshake_timeout_secs` (and the test fails if it is instead held for the long first-record
bound, which is how it detects the two collapsing back into one constant), a real rustls client
that handshakes and then says nothing is closed on `first_byte_timeout_secs`, and one that has
sent a record and gone quiet is closed on `idle_timeout_secs`.

**The last test is the regression, and it is deliberately the slow one.** The first-byte bound
was 60 seconds, and NetGet's own TLS client is precisely a peer that bound stranded:
`src/client/tls/mod.rs` completes the handshake inside `connect()` and then writes no application bytes at all until an action or `[ send message ]` says to, and a client made from the dashboard is routed `*` → manual — so it connects and
waits for a person, who gets 300 seconds (`src/state/intercepts.rs`). It is now 300s. Proving
that means holding a silent peer open **past 60 seconds with no startup parameters passed at
all**, so the wait cannot be made cheaper than the claim.

Every other test passes a short override instead of waiting the default out, which is also what
proves `first_byte_timeout_secs` and `idle_timeout_secs` are read rather than merely declared:
a parameter that was ignored would leave the 300-second default in force and the test would time
out. Each bound was verified by removing it and watching its test fail, and the default was
verified by putting 60 back and watching the regression test fail.
