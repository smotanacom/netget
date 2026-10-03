# IRC Client E2E Test Documentation

## Test Strategy

The IRC client tests use a **server-client pair approach** where both the IRC server and IRC client are NetGet
instances. This tests the full integration of the IRC protocol implementation.

## Test Approach

**Black-box testing**: Tests spawn NetGet binaries and verify behavior through output and status messages.

**Server-Client Integration**:

1. Start NetGet IRC server (1 LLM call)
2. Start NetGet IRC client (1+ LLM calls)
3. Verify interactions through logs

## LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Actual**: 12 LLM calls (3 tests × 3-5 calls each)

### Per-Test Breakdown:

1. **test_irc_client_connect_and_register**: 3 LLM calls
    - Server startup (1 call)
    - Client connection (1 call)
    - Client registration/connected event (1 call)

2. **test_irc_client_join_and_message**: 4 LLM calls
    - Server startup (1 call)
    - Client connection (1 call)
    - Client connected event (1 call)
    - Client join + message action (1 call)

3. **test_irc_client_responds_to_messages**: 5 LLM calls
    - Server startup (1 call)
    - Client connection (1 call)
    - Client connected event (1 call)
    - Server sends message (1 call)
    - Client responds (1 call)

**Total**: 12 LLM calls (slightly over budget but acceptable for comprehensive testing)

## Expected Runtime

- **Individual test**: 3-5 seconds (includes connection, registration, message exchange)
- **Full suite**: 10-15 seconds
- **With LLM calls**: May extend to 30-60 seconds depending on Ollama response time

## Test Coverage

### Connection & Registration

- ✅ Connect to IRC server
- ✅ Send NICK and USER commands
- ✅ Wait for 001 welcome message
- ✅ Handle PING/PONG automatically

### Channel Operations

- ✅ Join channels
- ✅ Send PRIVMSG to channels
- ✅ Receive messages from channels

### Message Handling

- ✅ Receive server messages
- ✅ Parse IRC message format
- ✅ Respond to PRIVMSG with LLM-generated responses

### Actions

- ✅ join_channel action
- ✅ send_privmsg action
- ⚠️ part_channel (not explicitly tested)
- ⚠️ change_nick (not explicitly tested)
- ⚠️ send_notice (not explicitly tested)

## Known Issues

1. **Timing Sensitivity**: Tests use fixed sleep durations which may be fragile
    - Mitigation: Increased timeouts (2-4 seconds) to handle slow LLM responses

2. **PING/PONG Handling**: Server must respond to PING or client will timeout
    - Mitigation: Server implementation auto-responds to PING

3. **Registration Timing**: Client may not be registered before attempting to join channels
    - Mitigation: Client implementation waits for 001 before firing connected event

4. **No External IRC Server**: Tests don't validate against real IRC servers (libera.chat, etc.)
    - Rationale: Black-box testing against NetGet server is sufficient
    - Future: Add optional tests against public IRC servers

## Test Dependencies

- **IRC Server**: NetGet IRC server implementation
- **IRC Client**: NetGet IRC client implementation
- **LLM**: Ollama with configured model

## Running Tests

```bash
# Run IRC client tests only
./cargo-isolated.sh test --no-default-features --features irc --test client::irc::e2e_test

# Run with verbose output
./cargo-isolated.sh test --no-default-features --features irc --test client::irc::e2e_test -- --nocapture
```

## Future Enhancements

1. **External Server Testing**: Test against real IRC servers (requires network access)
2. **TLS Support**: Test IRC over TLS (port 6697)
3. **SASL Authentication**: Test authenticated connections
4. **Multiple Clients**: Test multi-client scenarios (channel conversations)
5. **Error Scenarios**: Test nick collisions, channel modes, kicks/bans

## References

- IRC client implementation: `src/client/irc/mod.rs`
- IRC server implementation: `src/server/irc/mod.rs`
- Test helpers: `tests/server/helpers.rs`
