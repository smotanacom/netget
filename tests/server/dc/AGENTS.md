# DC (Direct Connect) Protocol E2E Tests

## Test Overview

Tests DC (Direct Connect) hub server implementation with NMDC protocol commands: authentication (Lock/Key), user
registration (ValidateNick/Hello), chat messages, and search functionality. Validates LLM's ability to handle DC
protocol semantics and hub management.

## Test Strategy

- **Isolated test servers**: Each test spawns separate NetGet instance with specific DC behavior
- **Raw TCP client**: Uses `tokio::net::TcpStream` for pipe-delimited NMDC protocol
- **No DC library**: Manual command construction and response parsing (tests actual protocol)
- **Command-focused**: Each test validates specific DC command handling
- **Action-based**: No scripting (tests LLM's DC protocol understanding)

## LLM Call Budget

- `test_dc_authentication()`: 2 LLM calls (ValidateNick + Key)
- `test_dc_hub_info()`: 2 LLM calls (ValidateNick + Key)
- `test_dc_chat()`: 3 LLM calls (ValidateNick + Key + chat message)
- `test_dc_search()`: 3 LLM calls (ValidateNick + Key + Search)
- `test_oversize_command_is_refused_not_buffered()`: 0 LLM calls (the bound is reached before
  any event is raised)
- `test_close_connection_actually_closes()`: 1 LLM call ($Quit)
- `test_pipe_in_a_message_cannot_inject_a_second_command()`: 1 LLM call (chat line)
- **Total: 12 LLM calls**, over the ~10 guideline by two. The three additions are each one call
  or none, and each pins a defect that had no test at all: an unbounded pre-auth accumulator, a
  `close_connection` that did not close, and `|` in an action field framing a second command

**Optimization**: Tests are already consolidated to minimize LLM calls. Each test includes authentication overhead (
ValidateNick + Key) but validates different DC features.

## Scripting Usage

❌ **Scripting Disabled** - Action-based responses only

**Rationale**: Tests validate LLM's understanding of DC protocol semantics (Lock/Key exchange, pipe delimiters, NMDC
command formats). Scripting would bypass this validation. For production DC hubs, scripting is recommended for common
patterns (Lock challenge, user list management).

## Client Library

- **tokio::net::TcpStream** - Raw async TCP socket
- **tokio::io::AsyncReadExt** - For reading bytes until pipe delimiter
- **tokio::io::AsyncWriteExt** - For sending DC commands
- **No DC library** - Manual message parsing

**Why raw TCP?**:

1. DC is pipe-delimited text protocol (simple to handle manually)
2. Tests actual byte-level protocol behavior
3. No Rust DC client libraries available on crates.io
4. Direct control over command timing and formatting

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~80-100 seconds for full test suite (4 tests)
    - Each test: ~2-3s server startup + 2-3 LLM calls × 5-8s per call
    - `test_dc_authentication`: ~20s (2 LLM calls)
    - `test_dc_hub_info`: ~20s (2 LLM calls)
    - `test_dc_chat`: ~25s (3 LLM calls)
    - `test_dc_search`: ~25s (3 LLM calls)

**Note**: DC tests are similar to IRC in speed because:

- No scripting (each command requires LLM call)
- Authentication requires multiple commands (ValidateNick, Key)
- Multiple test cases to cover protocol breadth

## Failure Rate

- **Medium** (~10-15%) - LLM may not always follow DC protocol correctly
- Common failures:
    - LLM doesn't send $Lock challenge on connection
    - LLM doesn't respond to $ValidateNick
    - LLM formats commands incorrectly (missing $ or pipe)
    - LLM doesn't respond at all (returns empty action list)
- Timeout failures: Rare (~2%) - LLM takes too long to respond

**Why higher than DNS?**: DC protocol has specific formatting requirements ($command|, Lock/Key exchange). LLM sometimes
struggles with exact protocol syntax.

## Test Cases

### 1. DC Authentication (`test_dc_authentication`)

**Tests**: DC hub authentication flow (Lock/Key exchange, ValidateNick, Hello)

- **Prompt**: "listen on port {port} via dc. When users send $ValidateNick, accept with $Hello. When users send $Key,
  acknowledge. Be a friendly DC hub named 'NetGet Hub'."
- **Client Actions**:
    1. Connect and read initial $Lock challenge from hub
    2. Send `$ValidateNick testuser|`
    3. Send `$Key fakekey123|` (hub should accept any key)
    4. Read responses
- **Expected**:
    - Initial response contains "$Lock"
    - Response to ValidateNick contains "$Hello testuser"
- **Purpose**: Tests basic DC authentication and user acceptance

**LLM Calls**: 2 (one for ValidateNick, one for Key)

**Notes**:

- Server automatically sends $Lock challenge on connection (no LLM call)
- Test uses fake key (no validation expected)
- If no Hello received, test fails (authentication is critical for DC)

### 2. DC Hub Info (`test_dc_hub_info`)

**Tests**: Hub information commands (HubName, HubTopic)

- **Prompt**: "listen on port {port} via dc. Accept all users. Send hub name 'NetGet DC Hub' and hub topic 'Test Hub' to
  new users after they send $ValidateNick."
- **Client Actions**:
    1. Connect and read $Lock
    2. Send `$ValidateNick testuser|`
    3. Send `$Key fakekey|`
    4. Read responses (up to 10 lines to catch all hub info)
- **Expected**: Response contains "$HubName" or "NetGet"
- **Purpose**: Tests hub information broadcasting

**LLM Calls**: 2 (ValidateNick + Key)

**Notes**:

- May receive multiple response lines (Lock, Hello, HubName, HubTopic)
- Test accepts hub name in any response line
- Tests loose assertion to handle LLM variations

### 3. DC Chat (`test_dc_chat`)

**Tests**: Public chat messages

- **Prompt**: "listen on port {port} via dc. Accept all users. When users send public chat messages (format: <nickname>
  message|), echo them back or respond with a greeting."
- **Client Actions**:
    1. Authenticate (ValidateNick + Key)
    2. Send `<testuser> Hello hub!|`
    3. Read response
- **Expected**: Response contains "Hello" or greeting
- **Purpose**: Tests chat message handling

**LLM Calls**: 3 (ValidateNick + Key + chat message)

**Notes**:

- DC chat format: `<nickname> message|`
- LLM may echo or respond differently
- Test accepts any response containing relevant keywords

### 4. DC Search (`test_dc_search`)

**Tests**: File search functionality

- **Prompt**: "listen on port {port} via dc. Accept all users. When users
  send $Search commands, respond with one fake search result: filename 'test.txt', size 1024 bytes, using $SR command."
- **Client Actions**:
    1. Authenticate (ValidateNick + Key)
    2. Send `$Search Hub:testuser F?F?0?1?test|` (search for "test")
    3. Read responses (up to 10 lines)
- **Expected**: Response contains "$SR" and "test"
- **Purpose**: Tests search result generation

**LLM Calls**: 3 (ValidateNick + Key + Search)

**Notes**:

- $SR format: `$SR source filename\x05size slots/slots\x05hubname|`
- The assertion is loose (a `$SR` containing "test"), but it is now an **assertion**: the test
  used to print "Note: Did not receive search result / This may be expected if search is not
  fully implemented" and pass. The mock answers every `$Search` with a `$SR`, so a hub that
  sent nothing was a real failure being reported as a note

### 5. Oversize Command (`test_oversize_command_is_refused_not_buffered`)

128 KiB with no `|` must be refused, not buffered. The pre-auth accumulator had no cap and no
timeout. **0 LLM calls.**

### 6. Close (`test_close_connection_actually_closes`)

`close_connection` must send EOF. Its `break` used to leave only the inner action loop, so the
hub logged "closing connection N" and carried on reading. **1 LLM call.**

### 7. Command Injection (`test_pipe_in_a_message_cannot_inject_a_second_command`)

A `|` in `send_dc_broadcast`'s `message` must be stripped, not framed. The assertion is about
framing rather than words: `$Kick victim` still arrives, as inert text inside one command, and
what must not appear is a second `|`. **1 LLM call.**

## Known Issues

### 1. LLM DC Protocol Variability

LLM may not always follow DC protocol exactly:

- Missing $ prefix on commands
- Missing pipe delimiter
- Incorrect Lock/Key exchange format
- Extra text or explanations in responses

**Mitigation**: Tests use loose assertions (`contains()` instead of exact match).

### 2. Lock Challenge Format

Server automatically sends $Lock on connection (hardcoded), not generated by LLM. This ensures all tests start with
valid Lock challenge.

**Reason**: Lock challenge is critical for DC protocol, and LLM might not generate it correctly every time.

### 3. No Key Validation

Tests send fake keys (e.g., "fakekey123") and expect acceptance. Server doesn't validate Lock/Key calculation.

**Rationale**: Key validation algorithm is complex. Tests focus on protocol flow, not cryptographic correctness.

### 4. Timing-Dependent Failures

Tests use 10-second timeout per read. Slow LLM responses can cause timeouts, especially when multiple LLM calls are
needed.

**Mitigation**: Timeout is generous (10s), but very slow systems might still timeout.

## Performance Notes

### Why No Scripting?

DC tests don't use scripting because:

1. **Protocol validation**: Need to test LLM's DC protocol understanding
2. **Hub state**: DC requires tracking users, search results (hard to script)
3. **Varied commands**: Each test exercises different DC command (script wouldn't help much)

However, production DC hubs should use scripting for common patterns:

- Lock challenge (already hardcoded in server)
- NickList updates
- Standard hub messages

### LLM Call Budget Met

Tests use exactly 10 LLM calls, meeting the guideline.

**Why efficient?**:

- Each test includes authentication (2 calls) + feature test (1 call)
- No redundant servers
- Minimal test cases covering core DC functionality

## Future Enhancements

### Test Coverage Gaps

1. **$MyINFO command**: Test user information broadcasting
2. **$NickList command**: Test user list generation
3. **$Quit command**: Test user disconnect notifications
4. **$ConnectToMe**: Test P2P connection initiation
5. **Private messages**: Test $To/$From private messaging
6. **Operator commands**: Test $Kick, $ForceMove
7. **Multiple users**: Test hub with multiple simultaneous connections
8. **OpList**: Test operator list broadcasting

### Scripting Mode Test

Add test with scripting enabled:

- Validate script handles Lock/Key automatically
- Test script maintains user list
- Measure throughput (should handle hundreds of commands/sec)

### Protocol Compliance Test

Add test suite that validates against NMDC specification:

- Lock/Key calculation algorithm
- Command format validation
- Pipe delimiter handling
- Special character escaping

### Client Library Development

Consider developing minimal DC client library for Rust:

- Would simplify test code
- Could be published to crates.io
- Would help other Rust DC projects

## References

- [NMDC Protocol Specification](https://nmdc.sourceforge.io/NMDC.html)
- [ADC Protocol](https://adc.sourceforge.io/ADC.html) (not implemented)
- [DC++ Official Site](https://dcplusplus.sourceforge.io/)
- [PtokaX DC Protocol Wiki](http://wiki.ptokax.org/doku.php?id=dcprotocol)
