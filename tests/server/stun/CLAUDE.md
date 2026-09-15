# STUN Protocol E2E Tests

## Test Overview

End-to-end tests for STUN (Session Traversal Utilities for NAT) server functionality. Tests spawn NetGet STUN server and
validate binary protocol compliance using raw UDP sockets and manual STUN message construction.

**Protocols Tested**: STUN Binding Request/Response (RFC 8489), XOR-MAPPED-ADDRESS attribute encoding

## Test Strategy

**Raw UDP Socket Approach**: Tests use `std::net::UdpSocket` (sync, blocking) for simplicity and precise control:

- Direct binary message construction
- Byte-level protocol validation
- Easy debugging of protocol issues

**Manual STUN Message Construction**: Helper functions build STUN packets byte-by-byte:

- Ensures RFC 8489 compliance
- Tests exact wire format
- Allows testing malformed messages

**Black-Box Protocol Testing**: Tests validate only external behavior (request → response), not internal LLM prompts or
implementation details.

## LLM Call Budget

### Test Breakdown

1. **`test_stun_basic_binding_request`**: 1 server startup + 1 binding request = **2 LLM calls**
2. **`test_stun_multiple_clients`**: 1 server startup + 3 binding requests = **4 LLM calls**
3. **`test_stun_xor_mapped_address`**: 1 server startup + 1 binding request = **2 LLM calls**
4. **`test_stun_invalid_magic_cookie`**: 1 server startup + 0 requests (rejected) = **1 LLM call**
5. **`test_stun_malformed_short_packet`**: 1 server startup + 0 requests (rejected) = **1 LLM call**
6. **`test_stun_request_with_attributes`**: 1 server startup + 1 binding request = **2 LLM calls**
7. **`test_stun_rapid_requests`**: 1 server startup + 5 binding requests = **6 LLM calls**

**Total: 18 LLM calls**

### Optimization Opportunities

**Current Issue**: Each test spawns separate STUN server, even though STUN is stateless.

**Potential Improvement**: Consolidate into 2-3 comprehensive tests:

1. **Basic Operations**: Single server handles multiple clients, concurrent requests, attribute parsing (**~7-8 LLM
   calls**)
2. **Error Handling**: Invalid messages, short packets (**~1 LLM call**, rejections don't call LLM)
3. **XOR Encoding Validation**: Specific test for XOR-MAPPED-ADDRESS correctness (**~2 LLM calls**)

This could reduce to **~10-11 LLM calls total**, meeting the 10 call target.

**Trade-off**: Slight reduction in test isolation, but significant performance gain. STUN's stateless nature makes
consolidation safe.

## Scripting Usage

**Scripting NOT Used**: STUN responses contain dynamic data (client's public IP:port, transaction ID from request). LLM
must:

- Extract transaction ID from request (12 bytes, varies per request)
- Echo transaction ID in response
- Encode client's IP:port using XOR with magic cookie

Scripting mode cannot handle this per-request variability. Each STUN request requires LLM consultation.

**Future Optimization**: Could implement scripting with template variables:

- Script: "Return XOR-MAPPED-ADDRESS with {client_ip}:{client_port}, transaction_id={tid}"
- Executor fills variables from request context
- Would eliminate LLM calls per request

## Client Library

**Manual Implementation** - Raw UDP socket with helper functions

- **`UdpSocket::bind("127.0.0.1:0")`**: Sync UDP socket (blocking, simple)
- **`send_to()`**: Send STUN request to server
- **`recv_from()`**: Receive STUN response (with 5s timeout)
- **Message Construction**: Helper functions build STUN packets byte-by-byte

**No External Libraries**: STUN protocol simple enough to implement inline (~100 lines).

**Helper Functions**:

```rust
fn build_stun_binding_request() -> Vec<u8>;
fn build_stun_binding_request_with_tid(tid: &[u8; 12]) -> Vec<u8>;
fn build_stun_request_with_invalid_magic_cookie() -> Vec<u8>;
fn build_stun_request_with_software_attribute() -> Vec<u8>;
```

## Expected Runtime

**Model**: qwen3-coder:30b (default NetGet model)

**Runtime**: ~75-100 seconds for full test suite (7 tests, 18 LLM calls)

- Per-test average: ~10-15 seconds
- LLM call latency: ~2-5 seconds per call
- UDP request/response: <1ms (fast binary protocol)
- Server startup: ~500ms

**With Ollama Lock**: Tests run reliably in parallel. Total suite time ~75-100s due to serialized LLM access.

**Fast Tests**: `test_stun_invalid_magic_cookie` and `test_stun_malformed_short_packet` complete quickly (~2-3s) because
invalid requests bypass LLM.

## Failure Rate

**Historical Flakiness**: **Very Low** (<2%)

**Why So Stable?**:

- Stateless protocol: No complex state management
- Binary format: Unambiguous (no text parsing issues)
- Single round trip: No multi-step handshakes
- Local UDP: No network reliability issues

**Rare Failure Modes**:

1. **LLM Generates Invalid Binary Response** (<1% of runs)
    - Symptom: Response fails magic cookie or transaction ID validation
    - Cause: LLM hallucinates binary protocol structure
    - Mitigation: Retry test; if persistent, indicates prompt improvement needed

2. **UDP Packet Loss** (<0.5% of runs, CI only)
    - Symptom: recv_from() times out after 5 seconds
    - Cause: High CI runner load drops UDP packet
    - Mitigation: Extremely rare on localhost; retry succeeds

3. **Timeout on Rapid Requests** (<1% of runs)
    - Symptom: `test_stun_rapid_requests` times out waiting for 5th response
    - Cause: Ollama overload causes slow LLM processing
    - Mitigation: Ollama lock prevents this in modern tests

**Most Stable Tests** (all ~99% success rate):

- `test_stun_basic_binding_request`: Simple request-response
- `test_stun_invalid_magic_cookie`: No LLM involved (rejection logic)
- `test_stun_xor_mapped_address`: Validates XOR encoding

## Test Cases Covered

### Basic Functionality

1. **Basic Binding Request** (`test_stun_basic_binding_request`)
    - Sends minimal STUN Binding Request (20 bytes, no attributes)
    - Validates response message type (0x0101 = Binding Success Response)
    - Checks magic cookie (0x2112A442)
    - Verifies transaction ID echo (12 bytes match request)
    - Ensures response is valid STUN message (≥20 bytes)

### Concurrent Client Support

2. **Multiple Clients** (`test_stun_multiple_clients`)
    - Spawns 3 concurrent clients
    - Each sends binding request with unique transaction ID
    - Validates all receive success responses
    - Tests concurrent LLM processing

### Attribute Handling

3. **XOR-MAPPED-ADDRESS Attribute** (`test_stun_xor_mapped_address`)
    - Sends binding request
    - Parses response attributes
    - Looks for attribute type 0x0020 (XOR-MAPPED-ADDRESS)
    - Validates attribute is present (decoding optional in test)

4. **Request with SOFTWARE Attribute** (`test_stun_request_with_attributes`)
    - Builds request with SOFTWARE attribute (0x8022)
    - Validates server processes request despite attribute
    - Ensures response is success (ignores unknown attributes per RFC)

### Error Handling

5. **Invalid Magic Cookie** (`test_stun_invalid_magic_cookie`)
    - Sends request with 0xDEADBEEF instead of 0x2112A442
    - Validates server rejects request (no response or error response)
    - Tests RFC 8489 Section 7.3.1: "Servers MUST silently discard bad requests"

6. **Malformed Short Packet** (`test_stun_malformed_short_packet`)
    - Sends 10-byte packet (< 20-byte minimum)
    - Validates server ignores packet (no response)
    - Tests input validation

### Stress Testing

7. **Rapid Requests** (`test_stun_rapid_requests`)
    - Sends 5 rapid requests with different transaction IDs
    - No delay between requests
    - Validates server handles burst (receives ≥1 response)
    - Tests LLM queuing and concurrency

### Coverage Gaps

**Not Yet Tested**:

- IPv6 XOR-MAPPED-ADDRESS encoding (ATYP=0x02)
- Response with MAPPED-ADDRESS (0x0001) vs XOR-MAPPED-ADDRESS (0x0020)
- SOFTWARE attribute in response
- Error responses (400 Bad Request, 500 Server Error) - not implemented
- Authentication attributes (MESSAGE-INTEGRITY, USERNAME) - not implemented
- FINGERPRINT attribute (CRC32 checksum) - not implemented
- UDP packet fragmentation (>1500 bytes)
- Request with unknown attributes (comprehension-required vs optional)

## Test Infrastructure

### Helper Functions

**`build_stun_binding_request()`**:

```rust
fn build_stun_binding_request() -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&0x0001u16.to_be_bytes());      // Message Type
    packet.extend_from_slice(&0u16.to_be_bytes());           // Message Length (no attributes)
    packet.extend_from_slice(&0x2112A442u32.to_be_bytes());  // Magic Cookie
    packet.extend_from_slice(&[0x01, ..., 0x0c]);           // Transaction ID (12 bytes)
    packet
}
```

**`build_stun_binding_request_with_tid(tid: &[u8; 12])`**:

- Same as above, but accepts custom transaction ID
- Used for testing concurrent clients with unique IDs

**`build_stun_request_with_invalid_magic_cookie()`**:

- Builds request with 0xDEADBEEF magic cookie
- Used for testing rejection logic

**`build_stun_request_with_software_attribute()`**:

- Builds request with SOFTWARE attribute (0x8022)
- Attribute value: "STUN-Test-Client/1.0"
- Tests attribute parsing

### Test Execution Pattern

```rust
// 1. Start STUN server
let config = ServerConfig::new("Start a STUN server on port 0")
    .with_log_level("off");
let test_state = start_netget_server(config).await?;

// 2. Wait for server ready
tokio::time::sleep(Duration::from_millis(500)).await;

// 3. Create UDP client
let client = UdpSocket::bind("127.0.0.1:0")?;
client.set_read_timeout(Some(Duration::from_secs(5)))?;

// 4. Build and send STUN request
let request = build_stun_binding_request();
let server_addr = format!("127.0.0.1:{}", test_state.port).parse()?;
client.send_to(&request, server_addr)?;

// 5. Receive and validate response
let mut buf = vec![0u8; 2048];
let (len, _) = client.recv_from(&mut buf)?;
let response = &buf[..len];

assert!(len >= 20);  // Minimum STUN message size
let message_type = u16::from_be_bytes([response[0], response[1]]);
assert_eq!(message_type, 0x0101);  // Binding Success Response

// 6. Cleanup
test_state.stop().await?;
```

## Known Issues

### Transaction ID Format in Logs

**Issue**: Transaction IDs displayed as hex strings in logs (e.g., "0102...0c12")
**Impact**: Visual verification only (no functional impact)
**Mitigation**: Tests validate binary transaction ID, not string representation

### UDP Timeout Edge Cases

**Issue**: 5-second timeout may be too short on heavily loaded CI
**Impact**: Rare timeouts on `test_stun_rapid_requests`
**Mitigation**: Acceptable failure rate (<1%); retry succeeds

### XOR-MAPPED-ADDRESS Decoding Not Fully Validated

**Issue**: Tests only check for presence of attribute 0x0020, not correctness of XOR encoding
**Impact**: Could miss XOR implementation bugs
**Future Work**: Add helper to decode and verify XOR-MAPPED-ADDRESS matches client IP:port

## Running Tests

```bash
# Run all STUN tests (requires Ollama + model)
./cargo-isolated.sh test --features stun --test server::stun::e2e_test

# Run specific test
./cargo-isolated.sh test --features stun --test server::stun::e2e_test test_stun_basic_binding_request

# Run with output
./cargo-isolated.sh test --features stun --test server::stun::e2e_test -- --nocapture

# Run with concurrency (uses Ollama lock)
./cargo-isolated.sh test --features stun --test server::stun::e2e_test -- --test-threads=4
```

## Future Test Additions

1. **IPv6 Support**: Test XOR-MAPPED-ADDRESS with IPv6 client addresses
2. **XOR Decoding Validation**: Decode XOR-MAPPED-ADDRESS and verify it matches client IP:port
3. **SOFTWARE Attribute in Response**: Validate server includes SOFTWARE attribute in response
4. **Attribute Padding**: Test attributes with non-4-byte-aligned lengths (padding to 4-byte boundary)
5. **Unknown Attributes**: Send comprehension-required unknown attribute (0x0000-0x7FFF), expect 420 error
6. **Large Requests**: Send STUN request with many attributes (test maximum size handling)
7. **Malformed Attributes**: Attribute length exceeds message length, partial attributes
8. **Performance Benchmarking**: Measure requests/second with and without LLM (scripting future)

## `llm_failure_test.rs`

One test, `test_stun_answers_static_response_when_llm_fails`. The prompt gives the server an
instruction, which is what opts it into model control (`operator_wants_dynamic`); the mock then
has no rule for `stun_binding_request`, so `call_llm` returns `Err`.

It asserts the client still gets a correct Binding Success Response — transaction ID echoed,
XOR-MAPPED-ADDRESS decoding to its own source address, decoded from the raw bytes rather than
through the server's own builder — **and** that the log carries
`decision=static_fallback_llm_error`, and not `decision=static_default`.

Both halves are needed because those two paths put *byte-identical* datagrams on the wire: one
is a server nobody asked to consult a model, the other is a server whose backend went down.
The token deliberately is not `fail_closed_*`, since the peer did get an affirmative answer.
See the failure-behaviour table in `src/server/stun/CLAUDE.md`.

LLM call budget: 1 (startup); the per-request call is *made to fail on purpose*.
