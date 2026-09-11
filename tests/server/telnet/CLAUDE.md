# Telnet Protocol E2E Tests

## Test Overview

Tests Telnet server implementation with various terminal interaction patterns: echo, prompts, multi-line input, and
concurrent connections. Validates LLM's ability to handle terminal-like text protocol.

## Test Strategy

- **Isolated test servers**: Each test spawns separate NetGet instance with specific Telnet behavior
- **Raw TCP client**: Uses `tokio::net::TcpStream` with `BufReader` for line-based text protocol
- **No Telnet library**: Manual command construction and response reading (tests actual protocol)
- **Text-focused**: Tests terminal interaction patterns (echo, prompts, multi-line)
- **Action-based**: No scripting (tests LLM's text handling ability)

## LLM Call Budget

- `test_telnet_echo()`: 1 LLM call (echo command)
- `test_telnet_prompt()`: 1 LLM call (help command)
- `test_telnet_multiple_lines()`: 3 LLM calls (3 lines sent)
- `test_telnet_concurrent_connections()`: 6 LLM calls (3 clients × 2 operations each, but 3 concurrent)
- **Total: 11 LLM calls** (slightly exceeds 10 limit)

**Optimization Opportunity**: Could consolidate into 2 comprehensive servers:

1. Server handling echo + prompt + multi-line (4-5 LLM calls)
2. Server handling concurrent clients (3 LLM calls)
3. Total: 7-8 LLM calls (within budget)

**Why not consolidated yet?**: Tests provide clear isolation of different terminal interaction patterns.

## Scripting Usage

❌ **Scripting Disabled** - Action-based responses only

**Rationale**: Tests validate LLM's ability to handle various text interaction patterns (echo, prompts, multi-line
input). Scripting would be appropriate for production Telnet servers with deterministic command/response patterns.

## Client Library

- **tokio::net::TcpStream** - Raw async TCP socket
- **tokio::io::AsyncBufReadExt** - Line-based reading (`BufReader`)
- **tokio::io::AsyncWriteExt** - For sending text
- **No Telnet library** - Manual text handling

**Why raw TCP?**:

1. Telnet protocol in NetGet is simplified to line-based text (no Telnet negotiation)
2. Tests actual text I/O behavior
3. No dependency on Telnet client libraries
4. Direct control over input/output

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~90-110 seconds for full test suite (4 tests)
    - Each test: ~2-3s server startup + variable LLM calls × 5-8s per call
    - `test_telnet_echo`: ~15s (1 LLM call)
    - `test_telnet_prompt`: ~15s (1 LLM call)
    - `test_telnet_multiple_lines`: ~35s (3 LLM calls)
    - `test_telnet_concurrent_connections`: ~30s (6 LLM calls but concurrent, some parallelism)

**Note**: Telnet tests are moderate speed (faster than IRC due to fewer LLM calls, slower than DNS/DoT/DoH due to no
scripting).

## Failure Rate

- **Low-Medium** (~5-10%) - LLM usually handles text echo correctly
- Common failures:
    - LLM doesn't echo message back (returns empty response)
    - LLM adds unexpected formatting or explanations
    - Timeout on slow LLM responses
- Rare failures: LLM closes connection unexpectedly

## Test Cases

### 1. Telnet Echo (`test_telnet_echo`)

**Tests**: Basic echo functionality

- **Prompt**: "listen on port {port} via telnet. Echo back any text you receive, line by line. Add '> ' prompt after
  each echo."
- **Client Actions**:
    1. Connect
    2. Send "Hello Telnet Server\n"
    3. Read response (10-second timeout)
- **Expected**: Response contains "Hello Telnet Server" or "Hello"
- **Purpose**: Tests basic text echo (most fundamental terminal operation)

**LLM Calls**: 1

**Notes**:

- Simple echo test validates core Telnet text I/O
- Uses `contains()` check (LLM might add prefix/suffix)
- Prompt asks for "> " but test doesn't validate (focus on echo)

### 2. Telnet Interactive Prompt (`test_telnet_prompt`)

**Tests**: Interactive command prompt

- **Prompt**: "listen on port {port} via telnet. Send a welcome message 'Welcome to NetGet Telnet' when clients connect,
  then show a '$ ' prompt. Echo commands back with 'You said: ' prefix."
- **Client Actions**:
    1. Connect
    2. Send "help\n"
    3. Read responses (up to 3 lines)
- **Expected**: At least one non-empty response received
- **Purpose**: Tests interactive session with welcome and command handling

**LLM Calls**: 1 (for "help" command; welcome might be sent on connection)

**Notes**:

- Test sends command but doesn't strictly validate welcome message (timing-dependent)
- Accepts any non-empty response (LLM might format differently)
- Focus is on interaction, not exact output

### 3. Telnet Multiple Lines (`test_telnet_multiple_lines`)

**Tests**: Multi-line input handling

- **Prompt**: "listen on port {port} via telnet. For each line received, respond with 'Line N: <content>' where N is the
  line number starting from 1."
- **Client Actions**:
    1. Connect
    2. Send "First line\n"
    3. Read response
    4. Send "Second line\n"
    5. Read response
    6. Send "Third line\n"
    7. Read response
- **Expected**: Receive response for each line sent
- **Purpose**: Tests stateful line counting and multi-turn interaction

**LLM Calls**: 3 (one per line)

**Notes**:

- Tests sequential message handling
- LLM must track line number (tests state management in conversation context)
- Small delay between lines to ensure sequential processing

### 4. Telnet Concurrent Connections (`test_telnet_concurrent_connections`)

**Tests**: Multiple simultaneous clients

- **Prompt**: "listen on port {port} via telnet. Handle multiple concurrent clients. Echo each message back with the
  client's message."
- **Client Actions**:
    1. Spawn 3 concurrent clients (tokio tasks)
    2. Each client:
        - Connects
        - Sends "Client N message\n"
        - Tries to read response (5-second timeout)
    3. Wait for all clients to complete
- **Expected**: At least some clients receive responses (reports success count)
- **Purpose**: Tests server's concurrency handling

**LLM Calls**: Up to 6 (3 clients × up to 2 operations), but concurrent so some parallelism

**Notes**:

- Tests concurrent connection handling (separate tokio tasks)
- Each client is independent
- Test reports how many succeeded (doesn't require all 3)
- Demonstrates Ollama lock serializing LLM calls

## Known Issues

### 1. Welcome Message Timing

Tests that expect welcome messages on connection (`test_telnet_prompt`) may not reliably receive them because:

- LLM might not send welcome without trigger message
- Current implementation doesn't support "send on connect" without client data
- Tests work around this by sending command first

**Future Enhancement**: Add support for connection-opened events (like TCP's `send_first`).

### 2. Prompt Validation Not Strict

Tests request prompts (e.g., "> ", "$ ") but don't validate their presence or format. Focus is on text response content,
not exact formatting.

**Reason**: LLM output is variable; strict prompt validation would cause false failures.

### 3. Telnet Protocol Testing Covers Stripping, Not Negotiation

`line_framing_test.rs` sends the preamble a real `telnet(1)` client opens with — `IAC DO
TERMINAL-TYPE`, `IAC WILL NAWS`, `IAC DO NAOCRD` (whose option byte is `0x0A`, so it doubles
as the newline-splitting trap) and a subnegotiation containing an embedded `0x0A` — and
asserts the handler was given the typed line **alone**, quoted back in brackets. It also
asserts an 8 KiB run with no newline is refused rather than buffered.

What is still untested, because the server does not do it: answering `WILL/WONT/DO/DONT`,
terminal type (`TTYPE`), window size (`NAWS`), echo control. The server strips those sequences
and never replies, so a real client stays in its default line mode.

### 4. Concurrent Test Variability

`test_telnet_concurrent_connections` may have variable success rates (1/3, 2/3, or 3/3 clients succeed) depending on:

- LLM response speed
- Ollama lock serialization
- System load

**Mitigation**: Test reports success count but doesn't fail if some clients timeout.

## Performance Notes

### Why No Scripting?

Telnet tests don't use scripting because:

1. **Pattern validation**: Need to test LLM's text handling (echo, prompts, multi-line)
2. **Interactive behavior**: Tests terminal-like interaction patterns
3. **Varied patterns**: Each test exercises different interaction style

However, production Telnet servers should use scripting for:

- Deterministic command/response patterns
- Interactive shell commands (help, status, exit)
- Echo servers

### LLM Call Budget Slightly Exceeded

Tests currently use 11 LLM calls, slightly exceeding the 10-call guideline.

**Consolidation Plan**:

```rust
// Comprehensive Telnet test (7-8 LLM calls)
let prompt = format!(
    "listen on port {} via telnet.
    - Echo received lines with 'Line N: ' prefix (track line numbers)
    - Support 'help' command: show available commands
    - Support multiple concurrent clients
    - Show '$ ' prompt after responses",
    port
);

// Test all behaviors against single server:
// - Echo test: send line, verify echo (1 call)
// - Multi-line: send 3 lines, verify numbering (3 calls)
// - Concurrent: 3 clients send simultaneously (3 calls)
// Total: 7 LLM calls (within budget)
```

This would reduce from 4 test functions to 1 comprehensive test.

### Concurrent Client Performance

`test_telnet_concurrent_connections` spawns 3 clients simultaneously, but Ollama lock serializes LLM calls. Actual flow:

1. All 3 clients connect and send messages (parallel)
2. LLM calls happen sequentially (Ollama lock)
3. Responses sent back (parallel)

Net result: ~6-8 seconds per LLM call × 3 = 18-24 seconds total (not 3× faster).

## Future Enhancements

### Test Coverage Gaps

1. **Connection-opened event**: Test welcome message sent on connect (no client message)
2. **Wait for more**: Test multi-line accumulation with single LLM response
3. **Close connection**: Test LLM-initiated disconnect
4. **Binary data**: Test non-text data handling (if needed)
5. **Large messages**: Test messages longer than buffer size
6. **Command parsing**: Test shell-like command parsing (help, exit, etc.)
7. **Error handling**: Test invalid input, malformed commands

### Telnet Protocol Testing

If full Telnet protocol is added:

1. Test IAC escape sequences
2. Test WILL/WONT/DO/DONT negotiation
3. Test terminal type negotiation (TTYPE)
4. Test window size negotiation (NAWS)
5. Test echo control

### Scripting Mode Test

Add test with scripting enabled:

- Validate script handles common commands (help, status, exit)
- Test script maintains state (line numbers, user sessions)
- Measure throughput (should handle thousands of commands/sec)

### Interactive Shell Simulation

Add test that simulates full shell interaction:

```
$ help
Available commands: help, status, date, exit

$ status
System OK

$ date
2025-10-31 12:00:00 UTC

$ exit
Goodbye!
[connection closed]
```

Validates multi-turn conversation and state tracking.

## Comparison to SSH Tests

Telnet tests are simpler than SSH tests because:

- **No encryption**: No TLS handshake overhead
- **No authentication**: No key exchange or password validation
- **Text-only**: No binary protocol complexity
- **No channels**: Single stream per connection

SSH provides security that Telnet lacks, but Telnet tests are faster to run.

## References

- [RFC 854: Telnet Protocol](https://datatracker.ietf.org/doc/html/rfc854)
- [RFC 855: Telnet Options](https://datatracker.ietf.org/doc/html/rfc855)
- [Wikipedia: Telnet](https://en.wikipedia.org/wiki/Telnet)
