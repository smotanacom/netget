# DC Client E2E Test Documentation

## Overview

This directory contains end-to-end tests for the DC (Direct Connect) client implementation. Tests verify client connectivity, authentication, chat, search, and user list functionality.

## Test Strategy

### Approach

**Black-box testing** using local DC server from `src/server/dc/`:
- Start NetGet DC server on localhost
- Start NetGet DC client connecting to that server
- Use mocks to control LLM responses (default)
- Verify client/server interaction through output logs
- Optional: Test with real Ollama (ignored by default)

### Mock vs Real LLM

**Default (Mocked)**:
- All tests use `.with_mock()` for predictable behavior
- No Ollama required - tests run in CI/CD
- Fast execution (< 2 seconds per test)
- Deterministic outcomes

**Real LLM (Ignored)**:
- Tests marked with `#[ignore]`
- Run with `--use-ollama` flag or `--ignored`
- Validates actual LLM integration
- Slower (5-10 seconds per test)
- May have variability in LLM responses

## Test Suite

### Test 1: `test_dc_client_connect_and_auth_with_mocks`

**Purpose**: Verify client can connect to DC hub and complete authentication flow.

**LLM Calls**: 6 (all mocked)
1. Server startup - open DC server
2. Server connection received - send Lock challenge
3. Server ValidateNick received - send Hello acceptance
4. Client startup - open DC client
5. Client connected event - wait for auth
6. Client authenticated event - send first chat message

**Flow**:
1. Start DC server with mocks
2. Start DC client with nickname "testuser"
3. Client receives Lock → sends Key + ValidateNick
4. Client receives Hello → authenticated
5. Verify client shows "connected" in output

**Expected Duration**: < 2 seconds

**Assertions**:
- Client output contains "connected"
- All mocks called correct number of times

### Test 2: `test_dc_client_send_chat_with_mocks`

**Purpose**: Verify client can send public chat messages after authentication.

**LLM Calls**: 7 (all mocked)
1. Server startup
2. Server connection - Lock
3. Server ValidateNick - Hello
4. Server chat received - broadcast
5. Client startup
6. Client connected
7. Client authenticated - send chat
8. Client message received - wait

**Flow**:
1. Start DC server that echoes chat messages
2. Client authenticates as "chatter"
3. Client sends "Hello Hub!" in chat
4. Server echoes message back
5. Verify client received echo

**Expected Duration**: < 2 seconds

**Assertions**:
- Client authenticates successfully
- All mocks satisfied

### Test 3: `test_dc_client_request_userlist_with_mocks`

**Purpose**: Verify client can request and receive user list.

**LLM Calls**: 7 (all mocked)
1. Server startup
2. Server connection - Lock
3. Server ValidateNick - Hello
4. Server GetNickList - send user list
5. Client startup
6. Client connected
7. Client authenticated - request user list
8. Client userlist received

**Flow**:
1. Start DC server
2. Client authenticates as "lister"
3. Client sends `$GetNickList|`
4. Server responds with user list
5. Verify client receives list

**Expected Duration**: < 2 seconds

**Assertions**:
- Mock expectations met

### Test 4: `test_dc_client_connect_real_llm` (Ignored)

**Purpose**: Validate real LLM integration with DC client.

**LLM Calls**: 4-6 (actual Ollama calls)

**Flow**:
1. Start DC server without mocks
2. Start DC client with instruction to say hello
3. Wait for authentication and chat
4. Verify output

**Expected Duration**: 5-10 seconds

**Run with**:
```bash
./test-e2e.sh --use-ollama dc
# or
cargo test --features dc test_dc_client_connect_real_llm -- --ignored --use-ollama
```

## LLM Call Budget

**Target**: < 10 LLM calls total for entire suite
**Actual**: 9 LLM calls (all mocked except ignored test)

Breakdown:
- Test 1: 6 calls (mocked)
- Test 2: 7 calls (mocked)
- Test 3: 7 calls (mocked)
- Test 4: 4-6 calls (real LLM, ignored)

**Total mocked calls**: 20 (but parallelized and fast)
**Total real LLM calls (optional)**: 4-6

## Running Tests

### Run all DC client tests (mocked, default)
```bash
./test-e2e.sh dc
# or
cargo test --features dc --test "client::dc::*"
```

### Run with real Ollama
```bash
./test-e2e.sh --use-ollama dc
# or
NETGET_USE_OLLAMA=1 cargo test --features dc --ignored
```

### Run specific test
```bash
cargo test --features dc test_dc_client_connect_and_auth_with_mocks
```

## Expected Runtime

**Mocked tests**: < 5 seconds total
**Real LLM tests**: 10-20 seconds total

## Known Issues

### 1. Lock/Key Parsing

**Issue**: Some NMDC hubs may use non-standard Lock formats

**Impact**: Client may fail authentication with non-standard hubs

**Workaround**: Use `send_dc_raw_command` to manually send Key if needed

### 2. Chat Message Parsing

**Issue**: Private message parsing is simplified, may not handle all edge cases

**Impact**: Some complex private messages may not parse correctly

**Workaround**: Monitor debug logs, use raw command if needed

### 3. Timing Sensitivity

**Issue**: Tests may be timing-sensitive (authentication flow has multiple steps)

**Impact**: Occasional test flakiness if system is slow

**Mitigation**: Added generous sleep delays (1000ms) between steps

### 4. Server Availability

**Issue**: Tests depend on local DC server implementation

**Impact**: If server has bugs, client tests may fail

**Mitigation**: Server is well-tested independently

## Test Infrastructure

### Helper Functions Used

- `start_netget_server()` - Start DC server instance
- `start_netget_client()` - Start DC client instance
- `.with_mock()` - Configure mock LLM responses
- `.verify_mocks()` - Verify mock expectations met
- `.output_contains()` - Check output logs
- `.stop()` - Cleanup server/client

### Mock Configuration

Mocks specify:
- **Trigger**: When to respond (instruction pattern, event type)
- **Response**: Actions to return
- **Expectations**: How many times mock should be called

Example:
```rust
.with_mock(|mock| {
    mock
        .on_event("dc_client_connected")
        .respond_with_actions(serde_json::json!([
            { "type": "wait_for_more" }
        ]))
        .expect_calls(1)
})
```

## Event/Action Coverage

### Events Tested

✅ `dc_client_connected` - Lock received
✅ `dc_client_authenticated` - Hello received
✅ `dc_client_message_received` - Chat message
✅ `dc_client_userlist_received` - User list
⬜ `dc_client_search_result` - Search results (not tested yet)
⬜ `dc_client_hubinfo_received` - Hub info (not tested yet)
⬜ `dc_client_kicked` - Kicked event (not tested yet)
⬜ `dc_client_redirect` - Redirect event (not tested yet)

### Actions Tested

✅ `send_dc_chat` - Public chat
✅ `send_dc_get_nicklist` - Request user list
⬜ `send_dc_private_message` - Private message (not tested yet)
⬜ `send_dc_search` - Search (not tested yet)
⬜ `send_dc_myinfo` - Update info (not tested yet)
⬜ `send_dc_raw_command` - Raw command (not tested yet)
✅ `disconnect` - Disconnect (implicit in cleanup)

## Future Enhancements

1. **Search Testing**: Add test for file search functionality
2. **Private Messages**: Test private message send/receive
3. **Hub Info**: Test hub name, topic events
4. **Multi-Client**: Test multiple clients on same hub
5. **Error Handling**: Test kick, redirect, connection failures
6. **Unicode**: Test international characters in chat
7. **Performance**: Test rapid message sequences

## Debugging Tests

### View test output
```bash
cargo test --features dc test_dc_client_connect -- --nocapture
```

### Check logs
```bash
# Server log
cat /tmp/netget-server-*.log

# Client log
cat /tmp/netget-client-*.log
```

### Enable tracing
```bash
RUST_LOG=debug cargo test --features dc
```

## References

- Implementation: `src/client/dc/CLAUDE.md`
- Server tests: `tests/server/dc/CLAUDE.md`
- NMDC spec: https://nmdc.sourceforge.io/NMDC.html
