# SIP Client E2E Test Documentation

## Test Strategy

**Approach**: Black-box testing using the NetGet binary. Tests verify SIP client functionality by spawning both SIP
server and client processes, then asserting on behavior and output.

**Test Philosophy**:

- Minimal LLM calls (< 10 per suite)
- Use scripting mode where possible for deterministic server responses
- Focus on core SIP methods: REGISTER, OPTIONS, INVITE
- Test against self-hosted NetGet SIP server for consistency

## Test Suite

### 1. test_sip_client_register

**Purpose**: Verify SIP client can REGISTER with a SIP server

**LLM Call Budget**: 4 calls

- Server startup (1 call for script generation)
- Client connection + REGISTER (1 call for initial connection, 1 call for processing response)
- Final state update (1 call)

**Expected Runtime**: ~2-3 seconds

- 500ms server startup
- 1500ms client REGISTER + response processing
- Cleanup

**Test Flow**:

1. Start SIP server accepting REGISTER on ephemeral port
2. Start SIP client with REGISTER instruction
3. Client sends REGISTER to server
4. Server responds 200 OK with Expires: 3600
5. Assert client shows "connected" or "SIP" in output
6. Cleanup both processes

**Assertions**:

- Client output contains connection confirmation
- Process exits cleanly

**Known Issues**: None

---

### 2. test_sip_client_options

**Purpose**: Verify SIP client can query server capabilities via OPTIONS

**LLM Call Budget**: 4 calls

- Server startup (1 call for script generation)
- Client connection + OPTIONS (1 call for initial connection, 1 call for processing response)
- Final state update (1 call)

**Expected Runtime**: ~2-3 seconds

- 500ms server startup
- 1500ms client OPTIONS + response processing
- Cleanup

**Test Flow**:

1. Start SIP server responding to OPTIONS with Allow header
2. Start SIP client with OPTIONS instruction
3. Client sends OPTIONS request
4. Server responds 200 OK with Allow: INVITE, ACK, BYE, REGISTER, OPTIONS
5. Assert client protocol is "SIP"
6. Cleanup both processes

**Assertions**:

- Client protocol matches "SIP"
- Process exits cleanly

**Known Issues**: None

---

### 3. test_sip_client_invite

**Purpose**: Verify SIP client can initiate calls with INVITE

**LLM Call Budget**: 4-5 calls

- Server startup (1 call for script generation)
- Client connection + INVITE (1 call for initial connection, 1 call for processing response)
- Possible ACK (1 call if implemented, currently not implemented)
- Final state update (1 call)

**Expected Runtime**: ~3-4 seconds

- 500ms server startup
- 2000ms client INVITE + response processing (longer timeout for SDP handling)
- Cleanup

**Test Flow**:

1. Start SIP server accepting INVITE with SDP answer
2. Start SIP client with INVITE instruction including SDP offer
3. Client sends INVITE with SDP body
4. Server responds 200 OK with SDP answer
5. Assert client shows "SIP", "INVITE", or "200" in output
6. Cleanup both processes

**Assertions**:

- Client output contains SIP/INVITE/200 indication
- Process exits cleanly

**Known Issues**:

- ACK after 200 OK not implemented (would complete 3-way handshake)
- Test passes without ACK since we're only verifying INVITE/response exchange

---

## Test Execution

### Running Tests

```bash
# Run all SIP client tests
./cargo-isolated.sh test --no-default-features --features sip --test client::sip::e2e_test

# Run specific test
./cargo-isolated.sh test --no-default-features --features sip --test client::sip::e2e_test -- test_sip_client_register --exact
```

### Prerequisites

1. **Ollama Running**: Tests require Ollama API for LLM calls
   ```bash
   # Verify Ollama is running
   curl http://localhost:11434/api/version
   ```

2. **Port Availability**: Tests use ephemeral ports via `{AVAILABLE_PORT}` placeholder

3. **Network Localhost**: Tests bind to 127.0.0.1 (no external network required)

4. **Feature Flag**: Tests are gated by `#[cfg(all(test, feature = "sip"))]`

### Test Isolation

- Each test uses unique ephemeral port via `{AVAILABLE_PORT}` placeholder
- Server and client run as separate processes (no shared state)
- Cleanup ensures processes terminate after test

## Performance Characteristics

### LLM Call Budget

**Total Budget**: < 10 LLM calls for all 3 tests
**Actual Usage**: ~12-13 calls (4-5 per test)

**Breakdown**:

- Script generation (server startup): 1 call per test
- Client connection event: 1 call per test
- Response processing: 1 call per test
- State updates/memory: 1 call per test

**Optimization Opportunities**:

- Use scripting mode for server (already implemented)
- Could reduce client LLM calls by batching actions
- Could use scripting mode for client (not yet implemented)

### Runtime

**Total Runtime**: ~7-10 seconds for all 3 tests

- test_sip_client_register: ~2-3s
- test_sip_client_options: ~2-3s
- test_sip_client_invite: ~3-4s

**Bottlenecks**:

- LLM response time: 500ms-2s per call
- SIP message parsing/generation: < 1ms
- Network localhost UDP: < 1ms

## Limitations and Known Issues

### 1. ACK Not Implemented

**Issue**: SIP client does not send ACK after receiving 200 OK to INVITE
**Impact**: Incomplete 3-way handshake (INVITE → 200 OK → ACK)
**Workaround**: Tests only verify INVITE/200 OK exchange, not full handshake
**Priority**: Medium (complete for RFC 3261 compliance)

### 2. No Authentication Testing

**Issue**: Tests do not cover digest authentication (401 challenges)
**Impact**: Cannot verify auth flows
**Workaround**: N/A (feature not implemented)
**Priority**: Low (suitable for future enhancement)

### 3. Simplified SDP

**Issue**: SDP bodies are minimal (no actual RTP streams)
**Impact**: Cannot verify real media negotiation
**Workaround**: Tests verify SDP presence/structure only
**Priority**: Low (SIP is signaling only, RTP out of scope)

### 4. UDP Only

**Issue**: No TCP or TLS transport tests
**Impact**: Cannot verify reliability or encryption
**Workaround**: N/A (TCP/TLS not implemented)
**Priority**: Low (UDP is primary SIP transport)

## Debugging Failed Tests

### Test Timeout

**Symptom**: Test hangs, eventually times out
**Possible Causes**:

1. Ollama not running
2. LLM infinite loop or stuck processing
3. Network port conflict

**Debug Steps**:

```bash
# Check Ollama
curl http://localhost:11434/api/version

# Check port availability
netstat -an | grep 5060

# Run test with verbose output
RUST_LOG=debug ./cargo-isolated.sh test --features sip --test client::sip::e2e_test -- test_sip_client_register --exact --nocapture
```

### Assertion Failure

**Symptom**: Test fails assertion (e.g., output doesn't contain "connected")
**Possible Causes**:

1. LLM generated unexpected response
2. Protocol parsing error
3. Timing issue (client output not captured in time)

**Debug Steps**:

```bash
# Check client output
# The test prints output on failure: "Output: ..."

# Increase timeout if timing issue
tokio::time::sleep(Duration::from_millis(2000)).await;  # Increase from 1500ms

# Run test multiple times to check for flakiness
for i in {1..10}; do ./cargo-isolated.sh test --features sip --test client::sip::e2e_test -- test_sip_client_register --exact; done
```

### Server/Client Crash

**Symptom**: Process exits unexpectedly
**Possible Causes**:

1. Panic in SIP parsing code
2. Network error
3. Missing dependency (unlikely with manual implementation)

**Debug Steps**:

```bash
# Run with backtrace
RUST_BACKTRACE=1 ./cargo-isolated.sh test --features sip --test client::sip::e2e_test -- test_sip_client_register --exact --nocapture

# Check logs
cat netget.log
```

## Future Test Enhancements

### Priority 1 (Complete Basic Coverage)

1. **Test ACK Handling**: Once ACK is implemented, add test for full INVITE→200→ACK flow
2. **Test BYE Request**: Verify client can terminate active sessions
3. **Test CANCEL**: Verify client can cancel pending INVITE

### Priority 2 (Edge Cases)

1. **Test Error Responses**: Verify client handles 403 Forbidden, 486 Busy Here, etc.
2. **Test 180 Ringing**: Verify client waits for final response after provisional
3. **Test Multiple Dialogs**: Verify client can handle multiple calls simultaneously

### Priority 3 (Advanced Features)

1. **Test Authentication**: Once digest auth implemented, test 401 challenges
2. **Test TCP Transport**: Once TCP implemented, test large message handling
3. **Test TLS (SIPS)**: Once TLS implemented, test encrypted signaling

## Test Maintenance

### When to Update Tests

1. **Protocol Changes**: If SIP client implementation changes (ACK, auth, etc.)
2. **Event Schema Changes**: If `sip_client_connected` or `sip_client_response_received` events change
3. **Action Schema Changes**: If SIP client actions (sip_register, sip_invite, etc.) change
4. **Performance Degradation**: If tests start taking significantly longer (> 15s per test)

### Test Review Checklist

- [ ] LLM call budget within limit (< 10 per suite)
- [ ] Runtime acceptable (< 30s total)
- [ ] All assertions meaningful and stable
- [ ] Cleanup properly releases resources
- [ ] Tests pass consistently (not flaky)
- [ ] Documentation matches implementation

## References

- SIP Server Tests: `/tests/server/sip/`
- SIP Server Implementation: `/src/server/sip/CLAUDE.md`
- SIP Client Implementation: `/src/client/sip/CLAUDE.md`
- RFC 3261: SIP - Session Initiation Protocol
- Test Infrastructure: `/tests/helpers/mod.rs`
