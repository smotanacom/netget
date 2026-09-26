# SMB Protocol E2E Tests

## Test Overview

Tests SMB2 file server using manually constructed SMB2 binary packets over TCP. Validates that NetGet can handle SMB
protocol operations with LLM-controlled authentication and file operations.

**Protocol**: SMB 2.1 (dialect 0x0210)
**Test Scope**: SMB2 Negotiate, Session Setup, connection handling, authentication
**Test Type**: Black-box, prompt-driven

## Test Strategy

### Manual SMB2 Packet Construction

Tests manually build SMB2 binary protocol packets:

- **Negotiate Protocol** - Offer SMB 2.1 dialect
- **Session Setup** - Guest authentication
- Parse responses for SMB2 signature and status codes

**Why manual?** No Rust SMB2 client library exists for testing.

### Consolidated Approach

Tests organized by SMB2 operation type:

1. **Negotiate Protocol** - Protocol version negotiation
2. **Session Setup** - Guest authentication flow
3. **Concurrent Connections** - Multiple simultaneous clients
4. **Server Responsiveness** - Verify server responds to SMB traffic
5. **Correct Stack** - Verify SMB stack initialization
6. **LLM-Controlled Auth** - Authentication via LLM actions
7. **Connection Tracking** - Verify UI connection tracking

Each test starts one server with specific behavior.

## LLM Call Budget

**Total Budget**: **14 LLM calls** (7 servers × 2 operations average)

### Breakdown by Test

1. **test_smb_negotiate**: 1 server startup + 1 negotiate = **2 LLM calls**
    - Prompt: Accept all guest connections
    - Request: SMB2 Negotiate

2. **test_smb_session_setup**: 1 server startup + 2 operations = **3 LLM calls**
    - Prompt: Allow guest authentication
    - Requests: Negotiate + Session Setup

3. **test_smb_concurrent_connections**: 1 server startup + 3 clients = **4 LLM calls**
    - Prompt: Handle multiple concurrent connections
    - Requests: 3 concurrent Negotiate operations

4. **test_smb_server_responsiveness**: 1 server startup + 1 negotiate = **2 LLM calls**
    - Prompt: Respond to all SMB2 requests
    - Request: Negotiate

5. **test_smb_correct_stack**: 1 server startup = **1 LLM call**
    - Prompt: Start SMB server via smb
    - No operations (just verify stack)

6. **test_smb_auth_llm_controlled**: 1 server startup + 2 operations = **3 LLM calls**
    - Prompt: Allow user 'alice', deny others
    - Requests: Negotiate + Session Setup (guest)

7. **test_smb_connection_tracking**: 1 server startup + 1 negotiate = **2 LLM calls**
    - Prompt: Start SMB server
    - Request: Negotiate
    - Check output for connection tracking

**Note**: Connection tracking test doesn't count additional LLM calls (checks output only).

**CRITICAL**: No scripting mode - each SMB operation requires LLM call.

## Scripting Usage

**Scripting Mode**: ❌ **NOT USED**

SMB2 operations currently require LLM call per request. Action-based responses used.

**Future Enhancement**: Implement scripting for SMB2 operations:

- Script handles Negotiate (fixed dialect response)
- Script handles Session Setup (deterministic guest auth)
- Script handles basic file operations (fixed responses)
- Reduce per-request LLM calls to zero

## Client Library

**TCP Client**: `std::net::TcpStream`

- Used for raw TCP communication
- Manual SMB2 packet construction
- No SMB library dependency

**Manual SMB2 Encoding**: Tests build packets manually:

- NetBIOS Session Service header (4 bytes)
- SMB2 header (64 bytes)
- Command-specific body (variable length)

**Packet Builders**:

- `build_smb2_negotiate()` - Negotiate Protocol request
- `build_smb2_session_setup()` - Session Setup request (guest)
- `parse_smb2_status()` - Extract status code from response

**Why manual?** No Rust SMB2 client library exists for testing.

## Expected Runtime

**Model**: qwen3-coder:30b
**Total Runtime**: ~70 seconds for full test suite

### Per-Test Breakdown

- **test_smb_negotiate**: ~10s (startup + 1 negotiate)
- **test_smb_session_setup**: ~15s (startup + negotiate + session setup)
- **test_smb_concurrent_connections**: ~15s (startup + 3 concurrent negotiates)
- **test_smb_server_responsiveness**: ~10s (startup + 1 negotiate)
- **test_smb_correct_stack**: ~5s (startup only, no operations)
- **test_smb_auth_llm_controlled**: ~15s (startup + negotiate + session setup)
- **test_smb_connection_tracking**: ~10s (startup + negotiate + output check)

**Factors**:

- No scripting = LLM call per SMB operation
- SMB2 binary parsing adds minimal overhead
- TCP transport is fast (milliseconds)

## Failure Rate

**Failure Rate**: **Medium** (~10-15%)

### Common Failure Modes

1. **LLM doesn't return expected auth action** - Missing smb_auth_success (5%)
2. **Malformed SMB2 response** - Invalid packet structure (3%)
3. **Timeout on LLM call** - Ollama overload (~5%)
4. **Connection tracking not in output** - Race condition (~2%)

### Known Flaky Tests

- **test_smb_session_setup** - Sometimes LLM doesn't return guest auth success (10%)
- **test_smb_auth_llm_controlled** - LLM may allow/deny unpredictably (5%)
- **test_smb_connection_tracking** - Output capture timing issues (2%)

### Mitigation

- Clear prompts specifying exact auth behavior
- 5-second timeouts on TCP operations
- Tests accept multiple valid SMB status codes (0x00000000, 0xC0000016)
- Graceful degradation on output validation

## Test Cases

### 1. SMB2 Negotiate Protocol

**Purpose**: Validate SMB version negotiation

**Test Flow**:

1. Start SMB server accepting guest connections
2. Send SMB2 Negotiate request (dialect 0x0210)
3. Parse response for SMB2 signature (0xFE 'S' 'M' 'B')
4. Validate status code (0x00000000 = success)

**Expected Result**:

- Valid SMB2 header in response
- Status 0x00000000 (success)

### 2. SMB2 Session Setup (Guest Authentication)

**Purpose**: Validate guest authentication flow

**Test Flow**:

1. Start SMB server allowing guest auth
2. Send Negotiate request
3. Send Session Setup request (guest, no credentials)
4. Validate status code (0x00000000 success or 0xC0000016 more processing)

**Expected Result**:

- Session established (status 0x00000000 or 0xC0000016)

### 3. Multiple Concurrent Connections

**Purpose**: Validate server handles concurrent clients

**Test Flow**:

1. Start SMB server
2. Spawn 3 concurrent client tasks
3. Each sends Negotiate request
4. Verify all receive valid SMB2 responses

**Expected Result**:

- All 3 clients receive SMB2 responses
- No connection refused errors

### 4. Server Responsiveness

**Purpose**: Validate server responds to SMB traffic

**Test Flow**:

1. Start SMB server
2. Connect via TCP
3. Send Negotiate request
4. Verify response received (even if not perfect SMB2)

**Expected Result**:

- Server sends response data
- No immediate connection close

### 5. Correct Stack

**Purpose**: Verify SMB stack initialization

**Test Flow**:

1. Start server with "via smb" in prompt
2. Verify server.stack contains "SMB"

**Expected Result**:

- Stack name is "SMB" (or "IP>TCP>SMB")

### 6. LLM-Controlled Authentication

**Purpose**: Validate LLM controls authentication decisions

**Test Flow**:

1. Start SMB server with auth rules (allow "alice", deny others)
2. Send guest Session Setup
3. Check if LLM allowed or denied

**Expected Result**:

- LLM processes auth request
- Status reflects LLM decision (success or denied)

**Note**: Guest username may not be "alice", so test is best-effort.

### 7. Connection Tracking

**Purpose**: Verify connections tracked in UI output

**Test Flow**:

1. Start SMB server
2. Establish connection and send Negotiate
3. Check server output for connection tracking indicators

**Expected Result**:

- Output contains "SMB connection", "connection from", or "bytes"
- Connection lifecycle visible in logs

## The declared inbound bound (`inbound_limit_test.rs`)

In-process, on `tests/helpers/inbound_limit.rs`: a mock model that approves the
SESSION_SETUP and answers everything else with no actions, counting calls. After a NEGOTIATE
(whose `MaxWriteSize` must equal `MAX_WRITE_SIZE`) and an approved SESSION_SETUP:

- a WRITE of exactly `MAX_WRITE_SIZE` bytes reaches the model and is not refused as too large;
- a WRITE header declaring `MAX_WRITE_SIZE + 1` is answered `STATUS_INVALID_PARAMETER`, the
  connection closes, and the model is called zero times;
- a fresh connection's SESSION_SETUP still reaches the model.

Verified by removal twice: without the `length > MAX_WRITE_SIZE` check the over-size WRITE
is never answered (the server waits for its payload); without `close_after_reply` the
connection stays open after the refusal.

## Known Issues

### Manual SMB2 Implementation

- Protocol complexity high (64-byte headers, binary encoding)
- Tests may not cover all SMB2 edge cases
- Real Windows clients may behave differently

### LLM Authentication Unpredictability

- LLM may interpret auth prompts differently
- Guest authentication may be allowed or denied unpredictably
- Tests use best-effort validation (accept multiple outcomes)

**Workaround**: Tests accept both success and "more processing" status codes.

### No File Operation Tests

- Tests only validate Negotiate and Session Setup
- No Tree Connect, Create, Read, Write, Close operations
- File operations not validated

**Future**: Add tests for full SMB2 file operation sequence.

### Connection Tracking Race Condition

- Output capture may miss connection tracking messages
- Tests check for presence but don't fail if missing

**Workaround**: Non-critical validation, informational only.

## Running Tests

```bash
# Build release binary with all features
./cargo-isolated.sh build --release --all-features

# Run SMB E2E tests
./cargo-isolated.sh test --features smb --test server::smb::e2e_test

# Run specific test
./cargo-isolated.sh test --features smb --test server::smb::e2e_test test_smb_negotiate
```

**IMPORTANT**: Always build release binary before running tests.

## Future Enhancements

### Scripting Mode

- Add scripting support for SMB2 operations
- Reduce LLM calls from 14 to 7 (startups only)
- Generate Python/JS handlers for Negotiate, Session Setup

### Full File Operation Tests

- Add Tree Connect → Create → Read → Write → Close sequence
- Test directory listings (Query Directory)
- Test file attributes (Query Info)
- Validate LLM-controlled file content

### Real SMB Client Testing

- Test with smbclient (Linux)
- Test with Windows Explorer (SMB mounting)
- Validate full SMB2 protocol compliance

### Authentication Variants

- Test NTLM authentication (if implemented)
- Test user authentication (not guest)
- Test authentication denial scenarios

### Error Handling Tests

- Test invalid SMB2 packets
- Test unsupported commands
- Test malformed requests

## References

- [MS-SMB2: Server Message Block Protocol](https://docs.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2)
- [Wireshark SMB2 Wiki](https://wiki.wireshark.org/SMB2)
- [Samba SMB Implementation](https://www.samba.org/)
- [smbclient Linux tool](https://www.samba.org/samba/docs/current/man-html/smbclient.1.html)
