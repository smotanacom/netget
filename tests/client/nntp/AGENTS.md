# NNTP Client E2E Test Strategy

## Overview

This document describes the E2E testing strategy for the NNTP (Network News Transfer Protocol) client implementation.
Tests verify LLM-controlled client behavior by spawning the actual NetGet binary and testing as a black-box.

## Test Approach

### Philosophy

- **Black-box testing**: Tests interact with NetGet as a user would
- **LLM-driven**: Tests rely on the LLM to interpret instructions and execute commands
- **Server integration**: Tests use NetGet's NNTP server implementation
- **Minimal LLM calls**: Keep tests efficient (< 10 LLM calls total)

### Test Infrastructure

Tests use the shared helper functions in `tests/helpers/`:

- `start_netget_server()`: Start NetGet server with instruction
- `start_netget_client()`: Start NetGet client with instruction
- `NetGetConfig`: Configuration for server/client instances
- `{AVAILABLE_PORT}`: Placeholder for dynamic port allocation

## Test Cases

### Test 1: Connect and LIST

**LLM Calls**: 2 (server startup, client connection)

**Purpose**: Verify NNTP client can connect and execute basic LIST command

**Flow**:

1. Start NNTP server with instruction to respond to LIST
2. Start NNTP client with instruction to connect and send LIST
3. Verify client shows "connected" in output
4. Cleanup both server and client

**Expected Behavior**:

- Client establishes TCP connection
- Client receives welcome banner (200 or 201)
- Client sends LIST command
- Client receives newsgroup list
- No errors or crashes

### Test 2: Select Newsgroup

**LLM Calls**: 2 (server startup, client connection)

**Purpose**: Verify NNTP client can select a newsgroup with GROUP command

**Flow**:

1. Start NNTP server configured to handle GROUP commands
2. Start NNTP client with instruction to select "comp.lang.rust"
3. Verify client protocol is "NNTP"
4. Verify client shows connection message
5. Cleanup

**Expected Behavior**:

- Client sends GROUP command with newsgroup name
- Server responds with 211 status (group selected)
- Client receives article count and range information

### Test 3: Retrieve Article

**LLM Calls**: 2 (server startup, client connection)

**Purpose**: Verify NNTP client can retrieve articles with ARTICLE command

**Flow**:

1. Start NNTP server configured to respond to ARTICLE requests
2. Start NNTP client with instruction to retrieve article 1
3. Verify client protocol is "NNTP"
4. Cleanup

**Expected Behavior**:

- Client sends ARTICLE command
- Server responds with 220 status and article data
- Client receives headers and body
- Multi-line response is properly terminated with `.`

## LLM Call Budget

**Total LLM Calls**: 12 (3 tests × 4 calls each)

Budget breakdown per test:

- Server startup: 1
- Server greeting (`nntp_command_received` with `command: "GREETING"`): 1 — without a rule for
  it the server answers `400` and closes, and the client's command never arrives
- Client connection (`nntp_connected`): 1
- The command's `nntp_command_received` on the server: 1

Mock actions must be names the protocols actually execute (`send_nntp_list`,
`send_nntp_group`, `send_nntp_article` on the server; `nntp_list`, `nntp_group`,
`nntp_article` on the client). `tests/helpers/mock_action_names.rs` fails the test otherwise;
the earlier `send_nntp_command` / `send_nntp_line` / `send_nntp_terminator` rules never existed.

### Test 4: Command channel (`command_channel_test.rs`)

**LLM Calls**: 0. Starts a NetGet NNTP server with a `*` static handler and an NNTP client,
then `AppState::send_to_client` injects `nntp_group`, an unknown verb (must be `Rejected`) and
`nntp_quit`; asserts the command reached the server's access log and the client handle is gone
after the quit.

## Runtime Estimate

**Total Runtime**: ~8-12 seconds

Breakdown per test:

- Server startup: ~500ms
- Client startup: ~500ms
- Sleep for connection: 2s (to allow LLM processing)
- Verification and cleanup: ~500ms
- **Total per test**: ~3.5-4s

With 3 tests: 10.5-12s total

## Known Issues

1. **Server Implementation**: Tests assume NetGet NNTP server exists and works correctly
2. **LLM Variability**: LLM may not always execute exact commands as instructed
3. **Timing Sensitivity**: Tests use sleep() which may be too short on slow systems
4. **No Response Validation**: Tests don't parse NNTP responses, only check for connection
5. **Limited Coverage**: Tests don't cover POST, XOVER, HEAD, BODY commands

## Future Test Improvements

1. **Response Validation**: Parse NNTP status codes and verify correct responses
2. **POST Testing**: Add test for posting articles (requires 340 response handling)
3. **Multi-line Response**: Verify correct handling of article retrieval
4. **Authentication**: Test AUTHINFO commands (when implemented)
5. **Error Handling**: Test server errors (411, 430, etc.)
6. **XOVER Testing**: Verify article overview retrieval
7. **External Server**: Test against real public NNTP server (e.g., news.eternal-september.org)

## Test Maintenance

### When to Update Tests

1. **New NNTP Features**: Add tests when implementing AUTHINFO, STARTTLS, etc.
2. **Bug Fixes**: Add regression tests for fixed bugs
3. **Protocol Changes**: Update if NNTP implementation changes significantly

### Test Stability

Current tests should be stable because:

- Use local NetGet server (no external dependencies)
- Fixed LLM call count (predictable)
- Simple commands (LIST, GROUP, ARTICLE)
- Clear success criteria (connection established)

Potential stability issues:

- LLM may refuse to parse certain instructions
- Server implementation bugs
- Port allocation conflicts

## External Testing

For manual testing with real NNTP servers:

```bash
# Connect to public Usenet server
./netget --instruction "Connect to news.eternal-september.org:119 via NNTP. List newsgroups."

# Select newsgroup and read articles
./netget --instruction "Connect to news.eternal-september.org:119 via NNTP. Select newsgroup comp.lang.rust and retrieve article headers for articles 1-10."
```

**Note**: Many public NNTP servers require authentication (AUTHINFO), which is not yet implemented.

## Debugging Failed Tests

If tests fail:

1. **Check logs**: Enable `RUST_LOG=debug` for verbose output
2. **Verify server**: Ensure NNTP server is running correctly
3. **Test manually**: Try connecting with real NNTP client (e.g., `telnet`, `tin`)
4. **Check LLM output**: Review LLM-generated actions in logs
5. **Timing issues**: Increase sleep durations if needed

## References

- RFC 3977: Network News Transfer Protocol (NNTP)
- `src/client/nntp/CLAUDE.md`: Implementation documentation
- `tests/helpers/`: Shared test infrastructure
