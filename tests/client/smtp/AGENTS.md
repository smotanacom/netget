# SMTP Client E2E Testing

## Overview

E2E tests for the SMTP client verify email sending capabilities using a real SMTP server. Tests are designed to minimize
LLM calls while ensuring robust functionality.

## Test Strategy

### Approach

**Black-box testing**: Tests spawn the actual NetGet binary and verify SMTP client behavior through observable outputs.

**Test Infrastructure**:

- **Local SMTP server**: Python's `smtpd` debugging server (port 1025)
- **No external services**: All tests run localhost-only
- **No email verification**: Tests verify client readiness, not delivery (delivery requires complex SMTP server setup)

### Test Server Options

1. **Python SMTP Debugging Server** (Preferred for E2E)
   ```bash
   python3 -m smtpd -n -c DebuggingServer localhost:1025
   ```
    - Pros: Built-in, no dependencies, prints all emails to console
    - Cons: No STARTTLS/auth support

2. **MailHog** (Alternative)
   ```bash
   docker run -p 1025:1025 -p 8025:8025 mailhog/mailhog
   ```
    - Pros: Web UI for email verification, SMTP auth support
    - Cons: Requires Docker

3. **FakeSMTP** (Alternative)
    - Pros: GUI, saves emails to disk
    - Cons: Java dependency

### Test Cases

#### 1. `test_smtp_client_connection`

**Purpose**: Verify SMTP client can connect to local server

**Steps**:

1. Start Python SMTP debugging server on port 1025
2. Start NetGet SMTP client with instruction to connect
3. Verify client output shows "SMTP" or "connected"

**LLM Calls**: 1 (client connection)
**Runtime**: ~1-2s

**Why**: Basic connectivity smoke test

---

#### 2. `test_smtp_client_email_preparation`

**Purpose**: Verify LLM can understand email composition instructions

**Steps**:

1. Start local SMTP server
2. Start NetGet SMTP client with email preparation instruction
3. Verify client protocol is "SMTP"

**LLM Calls**: 1 (client connection)
**Runtime**: ~1-2s

**Why**: Validates LLM instruction parsing for email composition

---

#### 3. `test_smtp_client_no_auth`

**Purpose**: Verify client works without authentication (local relay)

**Steps**:

1. Start local SMTP server (no auth)
2. Start NetGet SMTP client without credentials
3. Verify client is ready

**LLM Calls**: 1 (client connection)
**Runtime**: ~1-2s

**Why**: Many local SMTP servers don't require auth; ensures this works

---

## LLM Call Budget

**Total LLM calls**: 3 (1 per test)

**Budget breakdown**:

- Client connection/initialization: 1 call per test
- Email composition: Verified indirectly (no actual send due to action dispatch TODO)

**Why minimal**:

- Each test verifies a single aspect of SMTP client behavior
- No need for follow-up LLM calls (connection is one-shot)
- No chained actions (send_email action dispatch not yet implemented)

## Expected Runtime

- **Per test**: 1-2 seconds (server start + client connect + verification)
- **Full suite**: 3-6 seconds
- **Ollama model**: qwen3-coder:30b (default)

## Known Issues

### 1. Action Dispatch Integration

**Issue**: `send_email` async actions not yet dispatched by framework
**Impact**: Tests verify client readiness, not actual email sending
**Workaround**: Tests focus on connection and protocol detection
**Fix**: Requires framework-wide client async action dispatcher

### 2. STARTTLS Testing

**Issue**: Python SMTP debugging server doesn't support STARTTLS
**Impact**: Can't test TLS in E2E without additional infrastructure
**Workaround**: Unit tests or manual testing with real SMTP servers
**Fix**: Use MailHog or real SMTP server for TLS tests

### 3. Email Delivery Verification

**Issue**: Verifying email was actually delivered requires SMTP server introspection
**Impact**: Tests can't verify email content was sent correctly
**Workaround**: Tests verify client behavior, assume lettre works correctly
**Fix**: Use MailHog's API or email file inspection for delivery verification

## Test Maintenance

### Updating Tests

When modifying SMTP client:

1. **Connection changes**: Update `test_smtp_client_connection`
2. **Email composition**: Update `test_smtp_client_email_preparation`
3. **Authentication**: Add new test for auth scenarios

### Adding Tests

Future test ideas:

1. **Email sending E2E**: When action dispatch is implemented
2. **STARTTLS test**: Using MailHog or similar
3. **Authentication test**: Using authenticated SMTP server
4. **Multiple recipients**: Verify array of `to` addresses
5. **Error handling**: Test connection failures, auth failures

## CI/CD Considerations

**Python dependency**: Tests require Python 3 installed
**Docker alternative**: Use MailHog if Python not available
**Isolation**: Each test starts/stops its own SMTP server
**Port conflicts**: Uses port 1025 (non-privileged, unlikely conflicts)

## Manual Testing

For manual verification beyond E2E tests:

```bash
# 1. Start local SMTP server
python3 -m smtpd -n -c DebuggingServer localhost:1025

# 2. In another terminal, start NetGet
./cargo-isolated.sh run --no-default-features --features smtp

# 3. Open SMTP client (in NetGet prompt)
open_client smtp localhost:1025 "Send a test email"

# 4. Check Python SMTP server output for email
```

## Performance

**Fast tests**: All tests complete in < 10s total
**Minimal LLM usage**: 3 calls total (well under budget)
**No network**: All tests use localhost
**No cleanup issues**: Servers killed properly in each test
