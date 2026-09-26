# HTTP/2 E2E Testing

## Overview

End-to-end tests for the HTTP/2 protocol implementation using real HTTP/2 clients (reqwest with HTTP/2 prior knowledge).

## Test Strategy

Black-box testing approach where:

1. NetGet binary is spawned with HTTP/2 prompts
2. Real HTTP/2 client (reqwest) connects and sends requests
3. Responses are validated for correctness (status, headers, body)
4. HTTP/2-specific features like multiplexing are tested

## Client Library

- **reqwest** - HTTP client with HTTP/2 support
- Uses `http2_prior_knowledge()` for cleartext HTTP/2 (h2c)
- No TLS/ALPN negotiation required for testing
- Validates HTTP/2 version in responses

## Test Suite

### Test 1: Basic GET Requests (`test_http2_basic_get_requests`)

**Purpose**: Verify basic HTTP/2 request-response cycle with multiple routes

**Scenario**:

- Start HTTP/2 server with 4 routes (/, /api/users, /api/status, /nonexistent)
- Send GET requests to each route
- Validate status codes, HTTP/2 version, and response bodies
- Verify 404 handling for unknown routes

**LLM Calls**: 1 (server startup)

**Runtime**: ~5-7 seconds

- Server startup: 2-3s
- LLM processing: 2s
- Requests: 4 × ~0.5s = 2s

**Key Assertions**:

- All responses use HTTP/2 (not HTTP/1.1)
- Status codes match expected (200, 404)
- JSON responses parse correctly
- Content-Type headers set appropriately

### Test 2: POST with Body (`test_http2_post_with_body`)

**Purpose**: Verify HTTP/2 POST requests with request bodies

**Scenario**:

- Start HTTP/2 server with POST endpoints (/echo, /api/users)
- Send POST with text body to /echo
- Send POST with JSON body to /api/users
- Validate response includes request data

**LLM Calls**: 1 (server startup)

**Runtime**: ~5-6 seconds

- Server startup: 2-3s
- LLM processing: 2s
- POST requests: 2 × ~0.5s = 1s

**Key Assertions**:

- POST bodies received and processed by LLM
- 201 Created status for user creation
- Response contains data from request (echo or user name)
- HTTP/2 version confirmed

### Test 3: Multiplexing (`test_http2_multiplexing`)

**Purpose**: Verify HTTP/2 multiplexing (concurrent requests over single TCP connection)

**Scenario**:

- Start HTTP/2 server with simple JSON endpoint
- Send 3 concurrent GET requests using same client
- Validate all requests succeed simultaneously

**LLM Calls**: 1 (server startup) + 3 (concurrent requests, but over same connection)

**Runtime**: ~7-8 seconds

- Server startup: 2-3s
- 3 concurrent LLM calls: ~4-5s (processed in parallel by different LLM calls)

**Key Assertions**:

- All 3 requests return 200 OK
- All use HTTP/2 protocol
- Requests processed concurrently (HTTP/2 multiplexing benefit)

### Tests 4-5: Failure semantics (`failure_semantics_test.rs`)

**Purpose**: What the peer gets when netget cannot, or will not, answer.

Both point the server at a mock that answers the startup instruction and nothing else,
so every `http2_request` is a backend failure.

- `test_http2_answers_500_when_the_llm_fails` — the stream gets **500** promptly, with no
  `Retry-After` (503 + `Retry-After` is reserved for an *overloaded* backend, so a client
  can tell a retryable failure from a permanent one; HTTP/2 answered a flat 500 for both
  until this was split). The body is asserted to name `netget` and to contain none of the
  backend URL, model name, retry text or `Caused by` chain.
- `test_http2_refuses_an_oversized_request_body` — a body one byte over
  `http_common::MAX_REQUEST_BODY_BYTES` (8 MiB) is answered **413**, and `expect_calls(1)`
  on the startup rule proves it cost no LLM call.

**LLM Calls**: 1 each (startup only).

### Tests 6-9: Connection bounds (`connection_bounds_test.rs`)

**Purpose**: prove each bound in `src/server/http2/h2_server.rs` is applied, from the peer's
side. In-process (`ServerForm` + `AppState`), no mock backend: the LLM endpoint is a dead port
and every rule is static or manual, so **zero LLM calls**. The read bounds are set short through
the `first_byte_timeout_secs` / `idle_timeout_secs` startup parameters.

- A raw socket that sends nothing reads EOF at the first-byte bound, and no bytes before it.
- An `h2` client that completes one static-answered GET and then goes quiet sees its client
  connection end at the idle bound (a GOAWAY, then close) — not at the first-byte bound, which
  is set 60s and the wrong way round on purpose.
- A GET routed to `manual` (parked for a human) keeps its connection open well past both short
  bounds: the stream's task holds `ConnectionActivity` busy.
- 128 idle sockets fill the cap; the 129th reads `HTTP/1.1 503` + `Retry-After` and EOF;
  closing one admitted socket frees exactly one slot.

Each was verified by removing what it tests: the `peek` deadline (test 6 hangs to its 49s
window), the `watch_idle` arm (test 7 never ends), the `busy()` guard (test 8 sees the
connection closed under the parked request), and the cap raised to 100 000 (test 9 is never
refused).

**Runtime**: ~20s in parallel, dominated by the parked-request wait.

### Tests 10-14: Stream bounds (`stream_bounds_test.rs`)

**Purpose**: prove the SETTINGS and the shared body budget in `src/server/http2/h2_server.rs`
from the peer's side. In-process, no mock backend, **zero LLM calls**; requests that must stay
open are parked on a `manual` rule.

- **The SETTINGS frame carries the declared values**, read by a hand-written frame reader (not
  `h2`, which is the server's own library): `MAX_CONCURRENT_STREAMS` 100, `INITIAL_WINDOW_SIZE`
  65,535, `MAX_FRAME_SIZE` 16,384, `MAX_HEADER_LIST_SIZE` 32 KiB, and a stream-0 WINDOW_UPDATE
  opening the connection window to 1 MiB.
- **curl reads the same limit.** `curl --http2-prior-knowledge --trace-config http/2` completes
  a static-answered GET and logs `MAX_CONCURRENT_STREAMS: 100` — nghttp2, an independent HTTP/2
  stack, decoding our SETTINGS. The test fails, not skips, without curl.
- **The 101st stream is refused.** A raw client ignoring the SETTINGS opens 101 streams at once
  (HPACK from the static table only); exactly stream 201 is reset with `REFUSED_STREAM` (7), and
  exactly 100 requests are parked. `h2`'s own client honours the limit and would never send the
  101st, which is why this one is hand-written.
- **The streams of a connection share one 8 MiB body budget.** An `h2` client parks a 6 MiB POST,
  then sends 3 MiB on the same connection: that stream is answered `503` + `Retry-After: 1`
  within 15s and never parks, while the first stays parked.
- **The h2c upgrade path advertises the same limit** (compiled only with `http` too): curl
  `--http2` against the HTTP/1.1 server reads `MAX_CONCURRENT_STREAMS: 100` after the `101`.
  curl's request does not complete there — a pre-existing defect in the upgrade path, recorded
  in `src/server/http/CLAUDE.md` — so only the SETTINGS are asserted.

Verified by removal: handshaking with `h2::server::handshake` instead of `bounded_h2_builder()`
fails the SETTINGS, curl and 101st-stream tests (curl then reads `MAX_CONCURRENT_STREAMS:
4294967295`, and no stream is reset); doing the same on the h2c path fails the upgrade test;
disabling the budget check leaves the second POST parked and fails the budget test.

Run both feature sets:

```bash
./cargo-isolated.sh test --no-default-features --features http2 --test server -- \
    http2::stream_bounds --test-threads=100
./cargo-isolated.sh test --no-default-features --features http,http2 --test server -- \
    http2::stream_bounds --test-threads=100
```

## Total LLM Call Budget

- Test 1: 1 (startup) + 4 (requests) = 5 calls
- Test 2: 1 (startup) + 2 (requests) = 3 calls
- Test 3: 1 (startup) + 3 (requests) = 4 calls
- Tests 4-5: 1 (startup) each = 2 calls
- **Total**: 14 calls (tests run separately, servers not reused)

**Optimization**: Could reduce to 9 calls by:

1. Combining Test 1 & 2 routes into single server (8 requests = 1 + 8 = 9 calls)
2. Keep Test 3 separate for multiplexing demo (1 + 3 = 4 calls)
3. **New Total**: 9 + 4 = 13 calls

**Current Approach**: Keep tests separate for clarity (over the <10 target at 14 calls).
The failure tests cannot share a server with the others: they need a model that answers
*nothing*, which is the opposite of what the functional tests configure.

## Runtime Performance

**Total Runtime**: ~17-21 seconds (all 3 tests)

- Test 1: 5-7s
- Test 2: 5-6s
- Test 3: 7-8s

**Bottlenecks**:

- LLM response time: 2-3s per call
- Server startup: 2s per test
- Network round-trips: minimal (localhost)

## Known Issues

### 1. LLM Call Budget Slightly Over

- **Issue**: 14 total LLM calls vs. target <10
- **Impact**: Tests take ~20s instead of ~15s
- **Mitigation**: Tests are kept separate for clarity. Could be optimized if needed.
- **Resolution**: Acceptable for comprehensive coverage

### 2. HTTP/2 Prior Knowledge Required

- **Issue**: Client uses `http2_prior_knowledge()` for cleartext HTTP/2
- **Impact**: Real browsers require TLS + ALPN negotiation
- **Mitigation**: Tests focus on protocol behavior, not TLS
- **Resolution**: TLS *is* supported by the server (`tls_cert_manager`); what is missing
  is **ALPN advertisement**, so a browser will not select `h2` on its own however the
  test is written. Testing browser-shaped negotiation needs the server to advertise ALPN
  first — see `src/server/http2/CLAUDE.md`.

### 3. No Server Push Testing

- **Issue**: server push **is** implemented (`push_resource` action; `handle_h2_request`
  sends PUSH_PROMISE + push stream before the main response) and nothing tests it.
  `reqwest` cannot observe a push, which is why the existing tests do not.
- **Impact**: the whole push path is unexercised.
- **Resolution**: the `h2` crate's client exposes `push_promises()` and would work — but
  the server frames with `h2` too, so such a test asserts only that one crate
  round-trips through itself, which root `CLAUDE.md` names as the circular-evidence
  case. It would be coverage, not maturity evidence. A non-`h2` peer (curl, nghttp2) is
  what the protocol actually needs.

## Test Isolation

**Process Isolation**: Each test runs in separate process

- Separate NetGet binary spawned per test
- No shared state between tests
- Port allocation via `{AVAILABLE_PORT}` placeholder

**Connection Reuse**: Within each test

- Same reqwest client used for multiple requests
- HTTP/2 connection reused (multiplexing)
- Demonstrates real-world HTTP/2 usage

## Client Setup

```rust
// Create HTTP/2 client with prior knowledge (cleartext HTTP/2)
let client = reqwest::Client::builder()
    .http2_prior_knowledge()  // Skip ALPN negotiation, use HTTP/2 directly
    .build()?;

// Send request
let response = client
    .get(format!("http://127.0.0.1:{}/", port))
    .send()
    .await?;

// Verify HTTP/2 version
assert_eq!(response.version(), reqwest::Version::HTTP_2);
```

## Debugging Tips

### Failed Connection

If connection fails:

```bash
# Check server logs
cat ~/.config/netget/netget.log | grep HTTP/2

# Verify port is listening
ss -tlnp | grep <port>

# Test with curl (HTTP/2)
curl --http2-prior-knowledge http://127.0.0.1:<port>/
```

### Wrong HTTP Version

If server responds with HTTP/1.1 instead of HTTP/2:

- Check server logs for "HTTP/2 (h2c with push) server listening"
- Confirm the request reached `H2Server::spawn_with_push_support` — the server is built
  directly on the `h2` crate, not on hyper's `http2::Builder`
- Ensure client uses `http2_prior_knowledge()`

### Timeout Issues

If tests timeout:

- Check LLM is running (Ollama)
- Verify network connectivity to 127.0.0.1
- Increase timeout in test helpers

## Future Enhancements

### TLS Support Testing

When TLS is added to HTTP/2 server:

- Test ALPN negotiation (h2 protocol)
- Verify certificate validation
- Test HTTP/2 over TLS (standard browser behavior)

### Server Push Testing

Push is implemented; the test is what is missing. See Known Issue 3 for why the obvious
`h2`-client version would be circular evidence.

### Stream Prioritization

When prioritization is exposed:

- Test priority headers
- Verify response ordering
- Test weight and dependency

## References

- [reqwest HTTP/2 Documentation](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html#method.http2_prior_knowledge)
- [HTTP/2 Testing Best Practices](https://http2.github.io/)
- [Hyper HTTP/2 Examples](https://github.com/hyperium/hyper/tree/master/examples)
