# SOCKS5 Proxy E2E Tests

## Test Overview

End-to-end tests for SOCKS5 proxy functionality. Tests spawn NetGet SOCKS5 proxy and validate protocol operations using
a custom SOCKS5 client implementation. Covers handshake, authentication, CONNECT requests, data relay, and filtering.

**Protocols Tested**: SOCKS5 handshake, username/password authentication (RFC 1929), CONNECT command, IPv4/domain
targets

## Test Strategy

**Custom SOCKS5 Client**: Implemented from scratch in test code (`Socks5Client` struct) for precise protocol control and
debugging visibility. This allows:

- Testing exact SOCKS5 message formats
- Validating binary protocol compliance
- Triggering edge cases and error conditions
- Avoiding library behavior masking protocol issues

**Local Target Server**: Tests spawn simple HTTP server on localhost as CONNECT target. This ensures:

- No external dependencies (tests work offline)
- Predictable responses for validation
- Fast test execution (<100ms server startup)

**Phase-by-Phase Testing**: Tests validate each SOCKS5 protocol phase independently (handshake, auth, CONNECT, relay).

## LLM Call Budget

### Test Breakdown

1. **`test_socks5_basic_connect`**: 1 server startup + 1 CONNECT = **2 LLM calls**
2. **`test_socks5_auth_username_password`**: 1 server startup + 1 auth + 1 CONNECT = **3 LLM calls**
3. **`test_socks5_reject_connection`**: 1 server startup + 1 CONNECT = **2 LLM calls**
4. **`test_socks5_connect_domain`**: 1 server startup + 1 CONNECT = **2 LLM calls**
5. **`test_socks5_multiple_connections`**: 1 server startup + 3 CONNECTs = **4 LLM calls**
6. **`test_socks5_data_relay`**: 1 server startup + 1 CONNECT + HTTP request/response = **2 LLM calls** (pass-through,
   no MITM)
7. **`test_socks5_mitm_inspection`** (if implemented): 1 server startup + 1 CONNECT + N data chunks = **~10 LLM calls**

**Total: 15-25 LLM calls** (depending on MITM test inclusion)

### Optimization Opportunities

**Current Issue**: Each test creates separate proxy server with specific behavior.

**Potential Improvement**: Consolidate into 2-3 comprehensive tests:

1. **Basic Operations**: No auth, allow all, test IPv4 + domain + multiple connections in one server (**~4-5 LLM calls
   **)
2. **Authentication & Filtering**: Username/password auth, selective blocking (**~3-4 LLM calls**)
3. **MITM Inspection**: Enable MITM for specific target, test data modification (**~10 LLM calls**)

This could reduce to **~17-19 LLM calls total**, closer to the 10 call target.

**Challenge**: SOCKS5 requires state across connections (auth, then CONNECT), making consolidation more complex than
stateless protocols.

## Scripting Usage

**Scripting NOT Used**: SOCKS5 requires dynamic LLM decisions based on:

- Authentication credentials (username/password validation)
- Connection targets (domain-based filtering)
- Data content (MITM inspection)

Scripting mode doesn't provide sufficient context for these decisions. Per-connection LLM consultation necessary.

## Client Library

**Custom Implementation** - `Socks5Client` struct in test code

- Manual binary protocol construction for SOCKS5 messages
- Uses `tokio::io::AsyncReadExt` / `AsyncWriteExt` for raw TCP
- Methods:
    - `handshake_no_auth()` - Phase 1 negotiation
    - `handshake_with_auth(username, password)` - Phase 1+2 with credentials
    - `connect_ipv4(ip, port)` - Phase 3 CONNECT to IPv4
    - `connect_domain(domain, port)` - Phase 3 CONNECT to domain
    - `into_stream()` - Convert to raw TcpStream for Phase 4 data relay

**Why Not Use SOCKS5 Library?**:

- No mature async Rust SOCKS5 client library (most are sync or unmaintained)
- Custom implementation provides test clarity and debugging control
- ~200 lines of code, well worth the flexibility

**Target Server**: Simple axum HTTP server on localhost for CONNECT testing.

## Expected Runtime

**Model**: qwen3-coder:30b (default NetGet model)

**Runtime**: ~60-90 seconds for full test suite (6-7 tests, 15-25 LLM calls)

- Per-test average: ~10-15 seconds
- LLM call latency: ~2-5 seconds per call
- SOCKS5 handshake: ~5-10ms (fast binary protocol)
- CONNECT request: ~10-20ms to establish target connection

**With Ollama Lock**: Tests run reliably in parallel. Total suite time ~60-90s due to serialized LLM access.

## Failure Rate

**Historical Flakiness**: **Low-Medium** (~5-10%)

**Common Failure Modes**:

1. **Timeout on CONNECT** (~5% of runs)
    - Symptom: Client receives no response after CONNECT request
    - Cause: LLM slow to decide on connection policy, client times out
    - Mitigation: Increase read timeout to 10 seconds

2. **Authentication Rejection** (~2% of runs)
    - Symptom: LLM rejects valid credentials
    - Cause: Model misinterprets authentication prompt or applies overly strict policy
    - Usually self-corrects on retry

3. **Domain Resolution Failure** (~1% of runs)
    - Symptom: CONNECT to domain name fails with "host unreachable"
    - Cause: DNS resolution failure on CI runner, or NetGet can't resolve localhost domains
    - Mitigation: Use 127.0.0.1 instead of "localhost" when possible

4. **Binary Protocol Mismatch** (<1% of runs)
    - Symptom: Protocol parsing error (invalid SOCKS version, unexpected message format)
    - Cause: LLM generates malformed SOCKS5 response (rare)
    - Indicates LLM hallucination on binary protocol structure

**Most Stable Tests**:

- `test_socks5_basic_connect`: Simple allow-all policy, no auth complexity
- `test_socks5_data_relay`: Pass-through mode, no LLM per-packet inspection

**Occasionally Flaky**:

- `test_socks5_auth_username_password`: LLM may be overly strict or lenient
- `test_socks5_connect_domain`: DNS resolution timing issues

## Test Cases Covered

### Handshake and Authentication

1. **Basic CONNECT (No Auth)** (`test_socks5_basic_connect`)
    - Tests handshake with method 0x00 (no authentication)
    - Validates CONNECT to IPv4 target
    - Checks SOCKS5 reply format and success code (0x00)

2. **Username/Password Authentication** (`test_socks5_auth_username_password`)
    - Tests handshake with method 0x02 (username/password)
    - Sends authentication credentials
    - Validates LLM accepts correct credentials
    - Attempts CONNECT after successful auth

3. **Auth Rejection** (if implemented)
    - Tests LLM rejects invalid credentials
    - Validates auth status code 0x01 (failure)
    - Ensures connection closes after auth failure

### Connection Handling

4. **CONNECT Domain Name** (`test_socks5_connect_domain`)
    - Tests CONNECT with ATYP=0x03 (domain name)
    - Validates proxy resolves domain
    - Checks connection establishment to resolved IP

5. **Multiple Concurrent Connections** (`test_socks5_multiple_connections`)
    - Spawns 3 concurrent SOCKS5 clients
    - Each performs handshake + CONNECT to same proxy
    - Validates proxy handles concurrent connections correctly

6. **Connection Rejection** (`test_socks5_reject_connection`)
    - Tests LLM blocks connection based on target
    - Validates SOCKS5 reply code 0x02 (connection not allowed)
    - Ensures rejected connections close gracefully

### Data Relay

7. **Data Relay (Pass-Through)** (`test_socks5_data_relay`)
    - Establishes SOCKS5 tunnel to HTTP server
    - Sends HTTP GET request through tunnel
    - Validates response received correctly
    - Tests bidirectional data flow

8. **MITM Inspection** (if implemented)
    - Enables MITM mode for specific target
    - Sends data through tunnel
    - Validates LLM receives data inspection events
    - Tests data modification and forwarding

### Connection bounds (`connection_bounds_test.rs`)

In-process (`ServerForm` + `AppState`), model-free, **0 LLM calls**; each test stands up its own
loopback target with a plain `TcpListener`. The relay idle bound is set to 2s through
`idle_timeout_secs`.

1. **The cap** — 256 admitted, the 257th reads `05 FF`, closing one frees one slot.
2. **Silent both ways** — the client reads EOF at the idle bound, and so does the target side.
3. **Traffic only upstream** — a byte every 500ms client → target for four bounds, the target
   never answering: every byte arrives, the tunnel stays open, and it closes once both go quiet.
4. **Traffic only downstream** — the same, target → client: a download is a live tunnel.
5. **MITM chunk parked for a human** — with `mitm_by_default` and a `manual` rule on
   `socks5_data_to_target`, the tunnel survives four bounds with the chunk unforwarded; the
   test then answers the intercept (`forward_socks5_data`), the chunk reaches the target, and
   the tunnel is still open half a bound later.

Verified by removal: with the passthrough `watch_idle` arm disabled, 2 and 3 fail; with a
second, per-direction clock for the target side, 4 fails ("the client side was closed at
tick 4"); with the MITM `busy()` guards removed, 5 fails on the tunnel closing ~250ms after
the answer.

### Coverage Gaps

**Not Yet Tested**:

- IPv6 targets (ATYP=0x04)
- BIND command (not implemented in NetGet)
- UDP ASSOCIATE command (not implemented)
- Multiple authentication methods in handshake
- Proxy chaining (SOCKS5 → SOCKS5 → target)
- Malformed SOCKS5 messages (fuzzing)
- Target server connection failures
- Large data transfers (multi-megabyte files)

## Test Infrastructure

### Custom SOCKS5 Client

**`Socks5Client` Implementation**:

```rust
struct Socks5Client {
    stream: TcpStream,
}

impl Socks5Client {
    async fn connect(proxy_addr: &str) -> Result<Self>;
    async fn handshake_no_auth(&mut self) -> Result<()>;
    async fn handshake_with_auth(&mut self, user: &str, pass: &str) -> Result<bool>;
    async fn connect_ipv4(&mut self, ip: Ipv4Addr, port: u16) -> Result<bool>;
    async fn connect_domain(&mut self, domain: &str, port: u16) -> Result<bool>;
    fn into_stream(self) -> TcpStream;
}
```

**Binary Protocol Construction**:

- Uses byte slices and manual serialization for precise message format
- Example: `[SOCKS5_VERSION, CMD_CONNECT, 0x00, ATYP_IPV4, ...ip_bytes, ...port_bytes]`
- Validates response fields (version, reply code, address type)

### Test Helper Functions

**`start_test_http_server()`**:

- Spawns simple HTTP server on random port
- Returns single-response server (accepts one connection, sends 200 OK)
- Used as CONNECT target for data relay tests

**Test Execution Pattern**:

```rust
// 1. Start target HTTP server
let (http_port, _handle) = start_test_http_server().await?;

// 2. Start NetGet SOCKS5 proxy
let prompt = "listen on port 0 using SOCKS5 stack with no auth. Allow all connections.";
let server = helpers::start_netget_server(ServerConfig::new(prompt)).await?;

// 3. Create SOCKS5 client
let proxy_addr = format!("127.0.0.1:{}", server.port);
let mut client = Socks5Client::connect(&proxy_addr).await?;

// 4. Perform handshake
client.handshake_no_auth().await?;

// 5. CONNECT to target
let target_ip = Ipv4Addr::new(127, 0, 0, 1);
let connected = client.connect_ipv4(target_ip, http_port).await?;
assert!(connected);

// 6. Use tunnel for data transfer
let mut stream = client.into_stream();
stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await?;
// ... read response ...

// 7. Cleanup
server.stop().await?;
```

## Known Issues

### SOCKS5 Reply Format Variability

**Issue**: LLM may generate slightly non-standard SOCKS5 replies (e.g., wrong bound address format)
**Impact**: Tests may fail on strict binary protocol validation
**Mitigation**: Tests check only critical fields (version, reply code) and accept variations in bound address

### Domain Name Resolution

**Issue**: NetGet may fail to resolve "localhost" on some systems
**Impact**: `test_socks5_connect_domain` may fail
**Mitigation**: Use "127.0.0.1" as domain target instead

### Authentication Prompt Sensitivity

**Issue**: LLM authentication decisions vary based on prompt phrasing
**Impact**: Tests may get inconsistent auth results (sometimes too strict, sometimes too lenient)
**Mitigation**: Use explicit credential examples in prompts: "Accept username 'user' with password 'pass'"

### Binary Protocol LLM Hallucination

**Issue**: Rare LLM hallucinations generate invalid SOCKS5 binary responses
**Impact**: Client receives garbled protocol messages
**Mitigation**: Retry test, report if persistent (indicates prompt improvement needed)

## Running Tests

```bash
# Run all SOCKS5 tests (requires Ollama + model)
./cargo-isolated.sh test --features socks5 --test server::socks5::e2e_test

# Run specific test
./cargo-isolated.sh test --features socks5 --test server::socks5::e2e_test test_socks5_basic_connect

# Run with output
./cargo-isolated.sh test --features socks5 --test server::socks5::e2e_test -- --nocapture

# Run with concurrency (uses Ollama lock)
./cargo-isolated.sh test --features socks5 --test server::socks5::e2e_test -- --test-threads=4
```

## Future Test Additions

1. **IPv6 Support**: Test ATYP=0x04 IPv6 addresses
2. **Connection Failures**: Target unreachable, connection refused, DNS failure
3. **Authentication Edge Cases**: Empty username/password, Unicode characters, very long credentials
4. **MITM Data Modification**: Test LLM modifies data in MITM mode
5. **Stress Testing**: 100+ concurrent connections, measure memory/CPU
6. **Timeout Handling**: Slow target connections, idle connections
7. **Proxy Chaining**: SOCKS5 → another SOCKS5 → target
8. **Protocol Fuzzing**: Malformed messages, invalid address types, wrong version numbers
9. **Performance Benchmarking**: Compare pass-through vs MITM mode latency
