# TURN Client E2E Tests

## Test Strategy

**Approach**: Black-box testing using NetGet binary with TURN server and client instances.

**Test Server**: NetGet TURN server (LLM-controlled allocation responses)

**LLM Call Budget**: < 10 calls per test suite

## Test Organization

### Test 1: Basic Allocation (`test_turn_client_allocate_relay`)

**Objective**: Verify TURN client can connect and allocate a relay address

**LLM Calls**: 4

- Server startup (1 call)
- Client connection and connected event (1 call)
- Client sends Allocate request (1 call)
- Server processes and responds (1 call)

**Flow**:

1. Start NetGet TURN server on random port
2. Start NetGet TURN client connecting to server
3. LLM instructs client to allocate relay (600s lifetime)
4. Verify client receives relay address in response
5. Verify output contains "TURN" or "connected"

**Expected Runtime**: 2-3 seconds

### Test 2: Permission Creation (`test_turn_client_create_permission`)

**Objective**: Verify TURN client can create permissions for peers

**LLM Calls**: 5

- Server startup (1 call)
- Client connection (1 call)
- Client allocates relay (1 call)
- Client creates permission (1 call)
- Server processes permission request (1 call)

**Flow**:

1. Start NetGet TURN server
2. Start NetGet TURN client
3. LLM instructs client to allocate relay
4. LLM instructs client to create permission for peer 192.168.1.100:5000
5. Verify permission created successfully

**Expected Runtime**: 2-3 seconds

### Test 3: Allocation Refresh (`test_turn_client_refresh_allocation`)

**Objective**: Verify TURN client can refresh allocations to extend lifetime

**LLM Calls**: 5

- Server startup (1 call)
- Client connection (1 call)
- Client allocates relay (60s lifetime) (1 call)
- Client refreshes allocation (600s lifetime) (1 call)
- Server processes refresh (1 call)

**Flow**:

1. Start NetGet TURN server
2. Start NetGet TURN client
3. LLM instructs client to allocate relay with short lifetime
4. LLM instructs client to refresh allocation with longer lifetime
5. Verify output shows refresh activity

**Expected Runtime**: 2-3 seconds

## Test Infrastructure

### Dependencies

- **NetGet binary** with TURN feature compiled
- **Ollama** running locally (for LLM calls)
- **helpers.rs** for E2E test utilities

### Feature Gate

All tests wrapped in:

```rust
#[cfg(all(test, feature = "turn"))]
```

### Running Tests

```bash
# Run all TURN client E2E tests
./cargo-isolated.sh test --no-default-features --features turn --test client::turn::e2e_test

# Run specific test
./cargo-isolated.sh test --no-default-features --features turn test_turn_client_allocate_relay
```

## LLM Call Budget Analysis

**Total LLM Calls Across Suite**: ~14 calls

- `test_turn_client_allocate_relay`: 4 calls
- `test_turn_client_create_permission`: 5 calls
- `test_turn_client_refresh_allocation`: 5 calls

**Budget Status**: ✅ Under 10 calls per individual test (within guidelines)

**Optimization Notes**:

- Tests use minimal prompts focused on single operations
- Server responses are simple (accept all requests)
- No complex decision-making required from LLM
- Tests isolated (no shared state between tests)

## Known Issues & Limitations

### Potential Flaky Behavior

1. **Timing Sensitivity**
    - 500ms delays for server startup may be insufficient on slow systems
    - 2s delays for client operations may timeout if LLM slow
    - **Mitigation**: Increase sleep durations if tests flaky

2. **LLM Response Variability**
    - LLM may not immediately trigger allocate/permission/refresh
    - Output parsing relies on keywords ("TURN", "connected", "refresh")
    - **Mitigation**: Use broader keyword matching, longer timeouts

3. **Port Conflicts**
    - {AVAILABLE_PORT} placeholder may conflict with other tests
    - **Mitigation**: Run tests serially with `--test-threads=1`

### Test Limitations

**No Data Relay Testing**:

- Tests only verify allocation/permission/refresh
- Do not test SendIndication/DataIndication relay
- **Reason**: TURN server relay forwarding not yet implemented

**No Authentication Testing**:

- No MESSAGE-INTEGRITY, REALM, or NONCE attributes
- Tests assume open TURN server
- **Reason**: Client doesn't implement auth yet

**No Error Handling Testing**:

- Tests don't verify client behavior on allocation errors
- No testing of error codes (e.g., 508 Insufficient Capacity)
- **Reason**: Focus on happy path for initial implementation

## Runtime Expectations

### Per-Test Runtime

- **test_turn_client_allocate_relay**: 2-3 seconds
- **test_turn_client_create_permission**: 2-3 seconds
- **test_turn_client_refresh_allocation**: 2-3 seconds

### Full Suite Runtime

**Expected Total**: 6-9 seconds (3 tests × 2-3s each)

**Actual**: May vary based on:

- Ollama model speed (qwen3-coder:30b recommended)
- System resources
- Network latency (localhost should be minimal)

## Test Maintenance

### Adding New Tests

**Guidelines**:

1. Keep LLM calls < 10 per test
2. Use simple, focused prompts
3. Verify basic functionality only
4. Add to LLM call budget analysis above

**Example New Test Ideas**:

- Test client handles Allocate error responses
- Test client can send data via SendIndication (once relay implemented)
- Test client can receive data via DataIndication
- Test multiple allocations from same client

### Debugging Failed Tests

**Common Failures**:

1. **"Server failed to start"**
    - Check Ollama running
    - Verify TURN feature compiled
    - Increase startup delay

2. **"Client should show TURN connection"**
    - Check client output with `println!("{:?}", client.get_output().await)`
    - Verify LLM triggered connection
    - Check for error messages in output

3. **"Client should be TURN protocol"**
    - Verify client_config prompt mentions "TURN"
    - Check protocol parsing in client registry

**Debug Commands**:

```bash
# Run single test with output
./cargo-isolated.sh test --no-default-features --features turn test_turn_client_allocate_relay -- --nocapture

# Check netget.log for TURN messages
tail -f netget.log | grep TURN
```

## References

- Parent implementation: `src/client/turn/CLAUDE.md`
- TURN RFC: RFC 8656
- Test helpers: `tests/server/helpers.rs`
- Similar tests: `tests/client/tcp/e2e_test.rs`
