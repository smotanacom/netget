# Syslog E2E Test Documentation

## Overview

End-to-end tests for Syslog protocol server. Tests verify message parsing (RFC 3164 and RFC 5424), filtering by facility
and severity, and LLM-controlled message handling.

## Test Strategy

### Approach

**Black-box, prompt-driven testing**:

- Start NetGet with comprehensive Syslog prompt
- Send syslog messages via raw UDP sockets
- Verify server processes messages correctly
- No response validation (syslog is one-way UDP)

### Client

**Raw UDP socket**:

- Standard library `UdpSocket`
- Send RFC 3164 and RFC 5424 format messages
- No response expected (syslog is send-only from client perspective)

**Note**: The `logger` command (built-in on Linux/macOS) can also be used for manual testing:

```bash
# Send to NetGet syslog server on custom port
logger -n 127.0.0.1 -P 8514 -p user.error "Test error message"
logger -n 127.0.0.1 -P 8514 -p auth.warning "Auth warning test"
```

However, E2E tests use raw UDP for more control over message format.

## LLM Call Budget

**Target**: < 10 LLM calls per test suite

### Actual Usage

- **Initial server setup**: 1 LLM call (parse prompt and setup scripting rules)
- **Message processing**: 0 LLM calls (scripting mode handles all messages)

**Total**: **1 LLM call** ✅

### Optimization Techniques

1. **Scripting mode**: All message filtering done via scripting rules (no LLM calls per message)
2. **Single server instance**: Reuse same server for all test cases
3. **Comprehensive prompt**: One prompt with all filtering rules defined upfront

## Test Coverage

### Test Cases (9 total)

1. **Emergency kernel message** (priority `<0>`, facility=kernel, severity=emergency)
    - Verifies: Critical message detection, kernel facility recognition
    - Expected: Store and alert with "🚨 CRITICAL:" prefix

2. **Auth failure message** (priority `<37>`, facility=auth, severity=notice)
    - Verifies: Auth facility recognition, facility-based filtering
    - Expected: Store and show with "🔐 AUTH:" prefix (overrides severity filter)

3. **Daemon error message** (priority `<27>`, facility=daemon, severity=error)
    - Verifies: Error severity detection, daemon facility
    - Expected: Store and show with "❌ ERROR:" prefix

4. **User warning message** (priority `<12>`, facility=user, severity=warning)
    - Verifies: Warning severity detection
    - Expected: Store and show with "⚠️ WARNING:" prefix

5. **User info message** (priority `<14>`, facility=user, severity=info)
    - Verifies: Info severity detection, silent storage
    - Expected: Store silently (no alert)

6. **Debug message** (priority `<15>`, facility=user, severity=debug)
    - Verifies: Debug filtering, message dropping
    - Expected: Ignore/drop (no processing)

7. **RFC 5424 format message** (priority `<165>`, structured format)
    - Verifies: RFC 5424 parsing, version field, ISO 8601 timestamp
    - Expected: Parse and process correctly (same as RFC 3164)

8. **AuthPriv message** (priority `<86>`, facility=authpriv, severity=info)
    - Verifies: AuthPriv facility recognition (sudo logs)
    - Expected: Store and show with "🔐 AUTH:" prefix

9. **Critical local0 message** (priority `<130>`, facility=local0, severity=critical)
    - Verifies: Custom facility (local0), critical severity
    - Expected: Store and alert with "🚨 CRITICAL:" prefix

### Coverage Summary

- **Facilities tested**: kernel (0), user (1), auth (4), daemon (3), authpriv (10), local0 (16)
- **Severities tested**: emergency (0), critical (2), error (3), warning (4), notice (5), info (6), debug (7)
- **Formats tested**: RFC 3164 (BSD syslog), RFC 5424 (modern syslog)
- **Filtering logic**: Severity-based, facility-based (auth/kernel override), debug dropping

## Runtime Performance

### Expected Duration

- **Server startup**: 2-3 seconds (LLM parses prompt and configures scripting)
- **Message sending**: < 1 second (9 UDP sends with 500ms delays)
- **Server processing**: 0 seconds (scripting mode, no LLM calls)
- **Total**: **~5-6 seconds** ✅

### Performance Characteristics

- **Fast**: No LLM calls per message (scripting mode)
- **Scalable**: Can send hundreds of messages without LLM overhead
- **Deterministic**: Predictable runtime (no LLM response time variance)

## Known Issues

### 1. No Response Validation

**Issue**: Syslog is one-way (client → server), so tests can't verify server responses
**Impact**: Tests only verify server doesn't crash, can't check actual LLM behavior
**Workaround**: Manual inspection of server logs (`netget.log`) to verify correct filtering

**Future Enhancement**: Add test helper to read server log output and verify expected messages

### 2. UDP Packet Loss

**Issue**: UDP is unreliable, packets may be lost (especially under load)
**Impact**: Tests may occasionally fail due to dropped packets
**Mitigation**: Short delays between sends (500ms), localhost only (no network loss)

### 3. No Timestamp Validation

**Issue**: Timestamp parsing tested but not validated (syslog_loose extracts, but test doesn't verify)
**Impact**: Incorrect timestamp parsing wouldn't be caught
**Workaround**: Manual verification with known timestamps

### 4. Limited RFC 5424 Coverage

**Issue**: Only one RFC 5424 test case (no structured data, msgid, etc.)
**Impact**: Complex RFC 5424 features not tested
**Future Enhancement**: Add test with structured data `[exampleSDID@32473 key="value"]`

## Running Tests

### Prerequisites

1. **Build NetGet release binary** (E2E tests use built binary):
   ```bash
   ./cargo-isolated.sh build --release --no-default-features --features syslog
   ```

2. **Ollama running** with model available (e.g., `qwen3-coder:30b`)

### Run Syslog E2E Test

```bash
# Run Syslog E2E test only (recommended)
./cargo-isolated.sh test --no-default-features --features syslog --test server::syslog::e2e_test

# Or run with specific log level
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features syslog --test server::syslog::e2e_test
```

### Expected Output

```
✓ Syslog server started on 127.0.0.1:XXXXX

[Test 1] Send emergency kernel message
✓ Emergency message sent

[Test 2] Send auth failure message
✓ Auth message sent

...

✓ All Syslog tests passed!
  - Sent 9 messages across different facilities and severities
  - Tested RFC 3164 and RFC 5424 formats
  - Tested filtering by severity and facility
```

## Manual Testing

### Using `logger` Command (Linux/macOS)

```bash
# Start NetGet syslog server
netget "listen on port 8514 via syslog, log all messages"

# Send test messages from another terminal
logger -n 127.0.0.1 -P 8514 -p user.error "Test error message"
logger -n 127.0.0.1 -P 8514 -p auth.warning "Failed login attempt"
logger -n 127.0.0.1 -P 8514 -p kern.crit "Kernel critical error"
logger -n 127.0.0.1 -P 8514 -p daemon.info "Service started"
```

### Using `nc` (Netcat)

```bash
# Send raw RFC 3164 message
echo "<34>Oct 11 22:14:15 myhost myapp: Test message" | nc -u 127.0.0.1 8514

# Send RFC 5424 message
echo "<165>1 2024-01-11T22:14:15.003Z myhost myapp - - - Test message" | nc -u 127.0.0.1 8514
```

## Debugging

### View Server Logs

```bash
# Real-time log tailing
tail -f netget.log | grep -i syslog

# Search for specific messages
grep "Syslog received" netget.log
grep "facility" netget.log
```

### Common Issues

**Server doesn't start**:

- Check if port 514 is privileged (< 1024 requires root)
- Use port > 1024 for testing (e.g., 8514)

**Messages not received**:

- Verify firewall allows UDP/514
- Check server is bound to correct interface (0.0.0.0 vs 127.0.0.1)
- Use `tcpdump` to verify packets arrive: `sudo tcpdump -i lo udp port 8514 -X`

**Parsing failures**:

- Check message format (must start with `<priority>`)
- Verify priority is valid (0-191)
- Try both RFC 3164 and RFC 5424 formats

## Priority Calculation Reference

Priority = Facility * 8 + Severity

### Common Priorities

- `<0>` = kernel.emerg (0*8+0)
- `<13>` = user.notice (1*8+5)
- `<34>` = auth.crit (4*8+2)
- `<86>` = authpriv.info (10*8+6)
- `<130>` = local0.crit (16*8+2)
- `<165>` = local4.notice (20*8+5)

### Facilities (multiply by 8)

- 0=kernel, 1=user, 2=mail, 3=daemon, 4=auth, 10=authpriv, 16-23=local0-7

### Severities (add to facility*8)

- 0=emerg, 1=alert, 2=crit, 3=err, 4=warning, 5=notice, 6=info, 7=debug
