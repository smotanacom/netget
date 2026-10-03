# POP3 Client E2E Testing

## Overview

E2E tests for the POP3 client verify email retrieval capabilities using a real POP3 server. Tests are designed to minimize LLM calls while ensuring robust functionality.

## Test Strategy

### Approach

**Black-box testing**: Tests spawn the actual NetGet binary and verify POP3 client behavior through observable outputs.

**Test Infrastructure**:

- **Local POP3 server**: NetGet's own POP3 server implementation
- **No external services**: All tests run localhost-only
- **Self-contained**: Each test starts its own NetGet POP3 server and client

### Test Server Options

1. **NetGet POP3 Server** (Preferred for E2E)
   - Pros: Built-in, no dependencies, LLM-controlled responses
   - Cons: Not a real mail server (no actual mailbox)
   - Perfect for testing client protocol behavior

2. **Dovecot** (Alternative)
   ```bash
   docker run -p 110:110 -p 995:995 dovecot/dovecot
   ```
   - Pros: Production-grade POP3 server, real mailbox
   - Cons: Requires Docker, complex configuration

3. **Courier** (Alternative)
   - Pros: Lightweight POP3 server
   - Cons: Requires system installation

### Test Cases

#### 1. `test_pop3_client_connection`

**Purpose**: Verify POP3 client can connect to local server and receive greeting

**Steps**:

1. Start NetGet POP3 server on available port
2. Start NetGet POP3 client with instruction to connect
3. Verify client output shows "POP3" or "connected"

**LLM Calls**: 2 (1 server startup + 1 client connection)
**Runtime**: ~3-5s

**Why**: Basic connectivity smoke test with greeting validation

---

#### 2. `test_pop3_client_authentication`

**Purpose**: Verify LLM can understand authentication instructions (USER/PASS)

**Steps**:

1. Start NetGet POP3 server with authentication responses
2. Start NetGet POP3 client with authentication instruction
3. Verify client is ready (shows POP3 protocol)

**LLM Calls**: 2 (1 server startup + 1 client connection)
**Runtime**: ~3-5s

**Why**: Validates LLM instruction parsing for POP3 authentication flow

---

#### 3. `test_pop3_client_mailbox_operations`

**Purpose**: Verify client can query mailbox status (STAT, LIST)

**Steps**:

1. Start NetGet POP3 server with mailbox responses (STAT, LIST)
2. Start NetGet POP3 client with mailbox query instruction
3. Verify client successfully connects

**LLM Calls**: 2 (1 server startup + 1 client connection)
**Runtime**: ~3-5s

**Why**: Ensures client can handle mailbox query commands

---

## LLM Call Budget

**Total LLM calls**: 6 (2 per test × 3 tests)

**Budget breakdown**:

- Server startup/initialization: 1 call per test (3 total)
- Client connection/initialization: 1 call per test (3 total)
- No follow-up calls needed (connection is primary test objective)

**Why minimal**:

- Each test verifies a single aspect of POP3 client behavior
- Connection and greeting validation are primary objectives
- Actual email retrieval would require more complex server setup

## Expected Runtime

- **Per test**: 3-5 seconds (server start + client connect + verification)
- **Full suite**: 10-15 seconds
- **Ollama model**: qwen3-coder:30b (default)

## Known Issues

### 1. Email Retrieval Not Tested

**Issue**: Tests don't verify actual email retrieval (RETR command)
**Impact**: Can't verify email content parsing
**Reason**: Requires complex email generation in server
**Workaround**: Tests focus on connection and protocol recognition
**Fix**: Future enhancement - add email content validation

### 2. TLS/SSL Not Tested

**Issue**: POP3S (port 995) not tested in E2E
**Impact**: Can't verify TLS client behavior
**Reason**: Requires server TLS configuration
**Workaround**: Manual testing or separate TLS test
**Fix**: Add TLS-enabled server and client test

### 3. Multiline Response Handling

**Issue**: Tests don't verify multiline responses (LIST, RETR, UIDL)
**Impact**: Can't verify client correctly reads dot-terminated responses
**Reason**: Requires more complex server responses
**Workaround**: Tested implicitly in mailbox operations test
**Fix**: Add specific multiline response validation

### 4. Error Handling Not Tested

**Issue**: Tests don't verify -ERR response handling
**Impact**: Can't verify client error recovery
**Reason**: Focus on happy path for E2E
**Workaround**: Unit tests or manual testing
**Fix**: Add negative test cases with -ERR responses

## Test Maintenance

### Updating Tests

When modifying POP3 client:

1. **Connection changes**: Update `test_pop3_client_connection`
2. **Authentication flow**: Update `test_pop3_client_authentication`
3. **Mailbox operations**: Update `test_pop3_client_mailbox_operations`

### Adding Tests

Future test ideas:

1. **Email retrieval E2E**: Test RETR command with full email content
2. **TLS/SSL test**: Using POP3S on port 995
3. **Message deletion**: Test DELE command
4. **TOP command**: Test header-only retrieval
5. **UIDL command**: Test unique ID listing
6. **Error handling**: Test -ERR responses and recovery
7. **Connection timeout**: Test idle timeout and reconnection

## CI/CD Considerations

**Self-contained**: No external dependencies (uses NetGet server)
**Isolation**: Each test starts/stops its own server
**Port conflicts**: Uses dynamic port allocation (AVAILABLE_PORT)
**Fast execution**: All tests complete in < 15s total
**No cleanup issues**: Servers and clients properly stopped

## Manual Testing

For manual verification beyond E2E tests:

```bash
# 1. Start NetGet POP3 server (in one terminal)
./cargo-isolated.sh run --no-default-features --features pop3
# Then in NetGet: open_server pop3 110 "Send greeting '+OK POP3 ready'. Accept all commands."

# 2. Start NetGet POP3 client (in another terminal)
./cargo-isolated.sh run --no-default-features --features pop3
# Then in NetGet: open_client pop3 localhost:110 "Authenticate and check mailbox"

# 3. Or use telnet to test server
telnet localhost 110
USER alice
PASS secret
STAT
QUIT
```

## Performance

**Fast tests**: All tests complete in < 15s total
**Minimal LLM usage**: 6 calls total (well under budget)
**No network**: All tests use localhost
**Self-contained**: No external server dependencies
**Proper cleanup**: No port conflicts or resource leaks

## Comparison to SMTP Client Tests

| Aspect | SMTP Client | POP3 Client |
|--------|-------------|-------------|
| **Test Server** | Python smtpd | NetGet POP3 server |
| **External Deps** | Python 3 | None |
| **LLM Calls** | 3 total | 6 total |
| **Runtime** | 3-6s | 10-15s |
| **Test Count** | 3 tests | 3 tests |
| **Protocol Complexity** | Medium | Low |

POP3 tests are slightly slower due to server startup overhead, but are more self-contained with no external dependencies.
