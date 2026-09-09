# TCP Protocol E2E Tests

## Test Overview

Tests the raw TCP server implementation with FTP-like commands and custom protocols. Validates that the LLM can
construct protocol responses from scratch using raw TCP byte streams.

## Test Strategy

- **Isolated test servers**: Each test spawns a separate NetGet instance with specific instructions
- **Raw TCP client**: Uses `tokio::net::TcpStream` for direct socket communication
- **No FTP library**: Originally used suppaftp but it was too slow (>2 minutes per test)
- **Command-focused**: Tests individual protocol commands rather than full protocol flows
- **Fast validation**: 10-second timeout per operation

## LLM Call Budget

- `test_ftp_greeting()`: 1 LLM call (connection opened event)
- `test_ftp_user_command()`: 1 LLM call (USER command received)
- `test_ftp_pwd_command()`: 1 LLM call (PWD command received)
- `test_simple_echo()`: 1 LLM call (echo data received)
- `test_custom_response()`: 1 LLM call (PING command received)
- `test_wait_for_more_keeps_the_fragment()`: 2 LLM calls (first half → `wait_for_more`, second
  half → echo of the joined payload)
- **Total: 7 LLM calls** (under the 10 limit)

**Optimization Opportunity**: Could consolidate these into a single comprehensive server that handles all commands,
reducing to 1 startup call + 5 request calls = 6 total. However, current approach provides better isolation and clearer
failure diagnosis.

## Scripting Usage

❌ **Scripting Disabled** - Action-based responses only

**Rationale**: TCP tests use simple one-shot prompts that work well with action-based LLM responses. Scripting adds
complexity without significant benefit for these straightforward test cases.

## Client Library

- **tokio::net::TcpStream** - Raw async TCP socket
- **tokio::io::{AsyncReadExt, AsyncWriteExt}** - For read/write operations
- **No protocol library** - Manual command construction and parsing

**Why raw TCP?**:

1. Faster than protocol-specific clients (no library initialization overhead)
2. Tests actual byte-level LLM behavior
3. No dependency on external FTP/protocol libraries
4. Direct control over timing and command sequencing

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~50-60 seconds for full test suite (5 tests × ~10s each)
- Each test includes: server startup (2-3s) + LLM response (5-8s) + validation (<1s)

## Failure Rate

- **Low** (~5%) - Occasional LLM response format issues
- Most failures: LLM doesn't include expected keywords (e.g., "220", "ACK")
- Timeout failures: Rare (<1%) - usually indicates Ollama overload

## Test Cases

### 1. FTP Greeting (`test_ftp_greeting`)

- **Prompt**: Respond to CONNECT with FTP 220 greeting
- **Client**: Sends "CONNECT\r\n"
- **Expected**: Response starts with "220"
- **Purpose**: Tests LLM's ability to send banner without receiving data first (though test sends CONNECT to trigger
  response)

### 2. FTP USER Command (`test_ftp_user_command`)

- **Prompt**: Respond to USER with 331 password request
- **Client**: Sends "USER anonymous\r\n"
- **Expected**: Response starts with "331"
- **Purpose**: Tests basic FTP command parsing and response generation

### 3. FTP PWD Command (`test_ftp_pwd_command`)

- **Prompt**: Respond to PWD with 257 current directory
- **Client**: Sends "PWD\r\n"
- **Expected**: Response starts with "257"
- **Purpose**: Tests LLM's ability to handle different FTP commands

### 4. Simple Echo (`test_simple_echo`)

- **Prompt**: Echo data with "ACK: " prefix
- **Client**: Sends "Hello, LLM!"
- **Expected**: Response contains both "ACK" and "Hello, LLM!"
- **Purpose**: Tests basic data echo and transformation

### 5. Custom Response (`test_custom_response`)

- **Prompt**: Respond to PING with PONG
- **Client**: Sends "PING"
- **Expected**: Response contains "PONG"
- **Purpose**: Tests custom protocol implementation (non-FTP)

### 6. `wait_for_more` Retention (`test_wait_for_more_keeps_the_fragment`)

- **Prompt**: Buffer input until END, then echo everything
- **Client**: Writes `PART1-`, proves nothing comes back, then writes `PART2-END`
- **Expected**: the echo carries **both** halves
- **Purpose**: `wait_for_more` used to drop the payload it was shown, so the model never saw the
  first half again. The middle step — asserting a *silent* two-second gap after the first write
  — is what stops the test passing vacuously: without it, two writes coalesced into one read
  would satisfy the final assertion whether or not the fragment was retained.

## Known Issues

### 1. FTP Greeting Test Workaround

The FTP protocol typically sends a greeting immediately on connection. However, the test sends "CONNECT" to trigger the
response. This is a test artifact - in a real FTP server, the `send_first` flag would be used to send the banner without
waiting for client data.

**Reason**: The test helper doesn't support `send_first` flag configuration, so we work around it by sending a command.

### 2. LLM Response Variability

The LLM may add extra text, formatting, or explanations to responses. Tests use `contains()` or `starts_with()` checks
rather than exact matching to accommodate this variability.

Example: LLM might respond with "220 NetGet FTP Server - Welcome!" instead of exactly "220 NetGet FTP Server\r\n"

### 3. No Connection Cleanup Validation

Tests don't verify that connections are properly closed on the server side. They just stop the server process, which
forcibly closes all connections.

**Future Improvement**: Add tests for graceful connection closure with `close_connection` action.

## Performance Notes

### Why Not Use suppaftp?

Original tests used the `suppaftp` library for full FTP client operations. This was abandoned because:

- Each FTP operation required multiple LLM round-trips
- Full test suite took >2 minutes
- Most time spent waiting for LLM responses to FTP control channel
- Raw TCP tests achieve same validation in 1/3 the time

### Timeout Strategy

10-second timeout per read operation provides good balance:

- Allows for slow LLM responses (5-8s typical)
- Catches hung connections quickly
- Fails fast on misconfigured servers

## Future Enhancements

### Test Coverage Gaps

1. **Binary data**: No tests for binary protocol handling (`encoding: "hex"` in either direction)
2. **Concurrent connections**: No tests for multiple simultaneous clients
3. **Connection closing**: No tests for LLM-initiated `close_this_connection`, and none for the
   dashboard's injected `close_connection` (every other peer-bearing protocol has a
   `peer_inject_test`; TCP, which they were told to copy, has none)
4. **State persistence**: No tests for connection-specific memory/state
5. **Queue bound**: `MAX_QUEUED_BYTES` has no test; it needs a peer that streams for the length
   of a mocked LLM call

### Consolidation Opportunity

All five tests could be consolidated into a single server with comprehensive instructions:

```rust
let prompt = format!(
    "listen on port {} via tcp.
    - When CONNECT: respond '220 NetGet FTP Server\r\n'
    - When USER ...: respond '331 Password required\r\n'
    - When PWD: respond '257 \"/home/user\"\r\n'
    - When PING: respond 'PONG\r\n'
    - For other data: respond 'ACK: ' + received data",
    port
);
```

This would reduce from 5 server spawns to 1, saving ~10-15 seconds of test time.

## References

- [RFC 959: File Transfer Protocol (FTP)](https://datatracker.ietf.org/doc/html/rfc959)
- [Tokio TcpStream](https://docs.rs/tokio/latest/tokio/net/struct.TcpStream.html)
