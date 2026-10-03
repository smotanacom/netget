# SSH Agent Client E2E Tests

## Test Overview

Tests the SSH Agent client implementation. The client connects to an SSH Agent server (typically the system's ssh-agent
process) via Unix domain socket and can request identities and signature operations.

## Test Strategy

- **Mock server approach**: Tests use a mock SSH Agent server due to complexity of real SSH Agent setup
- **Unix socket communication**: Uses Unix domain sockets, not TCP/IP
- **Binary protocol**: SSH Agent wire format (uint32 length + message type + data)
- **LLM-controlled requests**: Tests verify the client can make requests based on LLM instructions

## Current Status

⚠️ **TESTS ARE PLACEHOLDERS** - Similar to server tests, these require Unix socket support in test helpers and a mock
SSH Agent server implementation.

### What Works

- ✅ Unit test for message format (`test_ssh_agent_message_format`)
- ✅ Example mock server implementation (`example_mock_ssh_agent_server`)

### What Needs Implementation

- ❌ Mock SSH Agent server for testing
- ❌ Test helper support for Unix socket client connections
- ❌ Integration tests marked with `#[ignore]`

## LLM Call Budget

- `test_ssh_agent_client_connect()`: 1 LLM call (client connection) - **NOT YET IMPLEMENTED**
- `test_ssh_agent_client_sign_request()`: 2 LLM calls (connect + sign request) - **NOT YET IMPLEMENTED**
- `test_ssh_agent_message_format()`: 0 LLM calls (pure unit test) - ✅ **IMPLEMENTED**
- `example_mock_ssh_agent_server()`: 0 LLM calls (documentation only) - ✅ **IMPLEMENTED**
- **Total: 3 LLM calls** (0 currently, 3 when fully implemented)

**Well under the 10 LLM call limit** even when fully implemented.

## Mock Server Requirement

Unlike TCP-based protocols, SSH Agent clients need to connect to an existing server. Real ssh-agent is:

- Complex to set up in tests
- Requires SSH key management
- Platform-specific behavior

**Solution**: Implement minimal mock SSH Agent server that:

1. Binds to Unix socket in temp directory
2. Accepts connections
3. Responds to REQUEST_IDENTITIES with empty key list
4. Responds to SIGN_REQUEST with dummy signature
5. Ignores other message types

The mock server is shown in `example_mock_ssh_agent_server` test.

## Test Cases

### 1. Client Connect (`test_ssh_agent_client_connect`) - PLACEHOLDER

- **Status**: Marked with `#[ignore]`, not yet runnable
- **Setup**: Mock SSH Agent server on temp Unix socket
- **Instruction**: "Connect to Unix socket X as SSH Agent client. Request the list of identities."
- **Expected**: Client connects, sends REQUEST_IDENTITIES, receives response
- **Validation**: Client output shows "connected" or "identities"
- **LLM Calls**: 1 (connection event)

### 2. Sign Request (`test_ssh_agent_client_sign_request`) - PLACEHOLDER

- **Status**: Marked with `#[ignore]`, not yet runnable
- **Setup**: Mock SSH Agent server with at least one dummy key
- **Instruction**: "Connect to Unix socket X as SSH Agent client. Request a signature for test data."
- **Expected**: Client lists keys, then sends SIGN_REQUEST for first key
- **Validation**: Client output shows "signature" or "signed"
- **LLM Calls**: 2 (connect + sign request)

### 3. Message Format (`test_ssh_agent_message_format`) - IMPLEMENTED ✅

- **Status**: Runnable, no LLM calls
- **Test**: Validates SSH Agent wire format construction
- **Verifies**:
    - Correct message structure (length + type)
    - Big-endian encoding
    - Proper type values
- **Purpose**: Ensures test infrastructure understands SSH Agent protocol

### 4. Mock Server Example (`example_mock_ssh_agent_server`) - IMPLEMENTED ✅

- **Status**: Runnable, documentation/example only
- **Purpose**: Shows how to implement a minimal SSH Agent mock server
- **Features**:
    - Creates Unix socket listener
    - Accepts connections in background task
    - Responds to REQUEST_IDENTITIES with empty list
    - Template for real test implementation

## Known Issues

### 1. No System SSH Agent Testing

Tests use mock server instead of system ssh-agent because:

- System agent may not be running
- Requires SSH key setup
- Platform-specific paths (`./tmp/ssh-*/agent.*` or system `/tmp/ssh-*/agent.*`)
- Security implications of test keys

**Trade-off**: Mock server is simpler but doesn't test real agent compatibility.

### 2. Limited Protocol Coverage

Mock server only implements:

- REQUEST_IDENTITIES (type 11) → IDENTITIES_ANSWER (type 12)
- SIGN_REQUEST (type 13) → SIGN_RESPONSE (type 14)

Not implemented:

- ADD_IDENTITY (type 17)
- REMOVE_IDENTITY (type 18)
- Key constraints
- Agent locking/unlocking

**Rationale**: These operations are less common and would exceed LLM budget.

### 3. Dummy Signatures Only

Mock server returns invalid signatures (just placeholder bytes). Real signature validation would require:

- Valid SSH keys
- Cryptographic operations
- Signature format parsing

**Acceptable**: Tests focus on LLM integration, not cryptographic correctness.

### 4. Socket File Cleanup

Unix socket files persist after tests. Need proper cleanup:

```rust
impl Drop for MockAgent {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
```

## Implementation Roadmap

### Phase 1: Mock Server

1. Create `MockSshAgent` struct
2. Implement Unix socket listener
3. Handle REQUEST_IDENTITIES and SIGN_REQUEST
4. Add proper cleanup logic

### Phase 2: Basic Tests

1. Update test helpers or use direct UnixStream
2. Implement `test_ssh_agent_client_connect`
3. Implement `test_ssh_agent_client_sign_request`
4. Remove `#[ignore]` attributes

### Phase 3: Enhanced Coverage (Optional)

1. Add multi-key test
2. Add error handling test (invalid socket path)
3. Add timeout test

## Alternative: Integration Test with Real Agent

Instead of mock server, could test against real ssh-agent:

```rust
#[tokio::test]
#[ignore = "Requires system ssh-agent"]
async fn test_with_real_agent() {
    // Find SSH_AUTH_SOCK from environment
    let socket_path = std::env::var("SSH_AUTH_SOCK").unwrap();

    // Connect client to real agent
    // Note: May fail if agent has no keys
}
```

**Pros**: Tests real compatibility
**Cons**: Unreliable, platform-dependent, requires user setup

## Comparison with Other Client Tests

| Protocol  | Client Type | Server Type | Complexity |
|-----------|-------------|-------------|------------|
| HTTP      | reqwest lib | hyper mock  | Low        |
| Redis     | Manual TCP  | Redis mock  | Medium     |
| SSH Agent | Manual Unix | Custom mock | **High**   |

SSH Agent is more complex than HTTP/Redis because:

- Binary protocol (not text-based)
- Unix sockets (not TCP)
- Stateful conversations (key management)
- No simple library for mock server

## References

- [IETF SSH Agent Draft](https://datatracker.ietf.org/doc/html/draft-ietf-sshm-ssh-agent-05)
- [Tokio UnixStream](https://docs.rs/tokio/latest/tokio/net/struct.UnixStream.html)
- [OpenSSH Agent Protocol](https://github.com/openssh/openssh-portable/blob/master/PROTOCOL.agent)
- [ssh-agent-lib](https://docs.rs/ssh-agent-lib/)
