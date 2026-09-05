# UDP Client E2E Test Documentation

## Overview

This directory contains end-to-end (E2E) tests for the UDP client implementation. Tests verify that the UDP client can
connect to servers, send datagrams, receive responses, and change target addresses as directed by the LLM.

## Test Strategy

**Approach:** Black-box testing using the actual NetGet binary

- Tests spawn real NetGet processes (client and server)
- LLM controls client behavior via natural language prompts
- Tests verify client output and behavior

**LLM Call Budget:** < 10 calls total across all tests

- Each test uses 2-3 LLM calls (server startup, client startup, optional response)
- Minimizes test runtime while ensuring comprehensive coverage

**Runtime:** ~10-15 seconds total for all tests (with LLM calls)

## Test Cases

### 1. `test_udp_client_connect_to_server`

**LLM Calls:** 2 (server startup, client startup)
**Runtime:** ~3-4 seconds

Tests basic UDP client connection:

- Starts a UDP server that echoes datagrams
- Starts a UDP client that sends "HELLO"
- Verifies client output shows socket is bound and ready
- Cleanup both instances

**Success Criteria:**

- Client output contains "ready" or "bound"
- No connection errors

### 2. `test_udp_client_send_datagram`

**LLM Calls:** 2 (server startup, client startup)
**Runtime:** ~3-4 seconds

Tests UDP client can send datagrams:

- Starts a UDP server that logs received datagrams
- Client sends "PING" datagram
- Verifies client protocol is "UDP"
- Verifies LLM controls datagram sending

**Success Criteria:**

- Client protocol matches "UDP"
- Client follows LLM instruction to send datagram

### 3. `test_udp_client_receive_and_respond`

**LLM Calls:** 3 (server startup, client startup, server response)
**Runtime:** ~4-5 seconds

Tests UDP client can receive and process datagrams:

- Server sends "PONG" in response to any datagram
- Client sends "PING" and waits for response
- Verifies client output shows received datagram

**Success Criteria:**

- Client output contains "datagram", "received", or "PONG"
- Client successfully processes server response

### 4. `test_udp_client_change_target`

**LLM Calls:** 3 (server1, server2, client startup)
**Runtime:** ~4-5 seconds

Tests UDP client can change target address:

- Starts two UDP servers on different ports
- Client sends to server1, then changes target to server2
- Verifies client can dynamically change targets

**Success Criteria:**

- Client protocol is "UDP"
- Client successfully sends to multiple targets

## Running Tests

### Prerequisites

```bash
# Build NetGet with UDP feature
./cargo-isolated.sh build --release --no-default-features --features udp

# Ensure Ollama is running
```

### Run All UDP Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features udp --test client::udp::e2e_test
```

### Run Specific Test

```bash
./cargo-isolated.sh test --no-default-features --features udp --test client::udp::e2e_test -- test_udp_client_connect_to_server
```

### Run with Output

```bash
./cargo-isolated.sh test --no-default-features --features udp --test client::udp::e2e_test -- --nocapture
```

## Test Infrastructure

**Helper Functions** (from `tests/helpers/`):

- `start_netget_server()` - Spawn NetGet server process
- `start_netget_client()` - Spawn NetGet client process
- `get_available_port()` - Find unused port for testing
- `NetGetInstance::output_contains()` - Check output for substring
- `NetGetInstance::stop()` - Clean shutdown of process

**Port Allocation:**

- Tests use `{AVAILABLE_PORT}` placeholder in prompts
- Automatically replaced with available port from `get_available_port()`
- Prevents port conflicts between concurrent tests

**Cleanup:**

- All tests call `stop()` on server and client instances
- Ensures no stray processes after test completion
- Timeouts prevent hung tests

## Known Issues

1. **Timing Sensitivity:**
    - UDP is connectionless, so "connection" timing is different from TCP
    - Tests use 500ms delays to ensure server/client are ready
    - Increase delays if tests fail intermittently

2. **LLM Variability:**
    - LLM may use different phrasing for "ready" state
    - Tests check multiple output patterns ("ready", "bound")
    - If tests fail, check actual client output for alternative phrases

3. **Port Conflicts:**
    - Tests bind to OS-assigned ports to avoid conflicts
    - If conflicts occur, ensure `get_available_port()` is working correctly

4. **Datagram Loss:**
    - UDP does not guarantee delivery
    - Tests may fail if datagrams are lost (rare on localhost)
    - Retry tests if intermittent failures occur

## Future Enhancements

1. **Multi-source Testing:** Test client receiving from multiple sources
2. **Timeout Testing:** Verify client handles response timeouts
3. **Large Datagram Testing:** Test max datagram size (65KB)
4. **Broadcast/Multicast:** Test special addressing modes
5. **Error Handling:** Test invalid addresses, socket errors

## Debugging

**View Test Output:**

```bash
./cargo-isolated.sh test --no-default-features --features udp --test client::udp::e2e_test -- --nocapture
```

**Check NetGet Logs:**
Tests create log files in `./tmp/netget-test-*` directories (if logging enabled).

**Manual Test:**

```bash
# Terminal 1: Start UDP server
./target/release/netget
> open_server 127.0.0.1:8080 "UDP echo server"

# Terminal 2: Start UDP client
./target/release/netget
> open_client 127.0.0.1:8080 "Send HELLO via UDP"
```

## Performance

**Current:** ~10-15 seconds for all 4 tests (with LLM calls)
**Without LLM:** ~2-3 seconds (if LLM calls were mocked)

**Bottleneck:** LLM inference time (~2-3s per call)
**Optimization:** Tests use an in-process mock LLM, so no real model is loaded at all

## Contribution Guidelines

When adding new tests:

1. **Minimize LLM calls** - Stay under 10 total across all tests
2. **Use clear prompts** - LLM should understand intent immediately
3. **Verify output** - Check multiple output patterns for robustness
4. **Clean up** - Always call `stop()` on instances
5. **Document** - Update this CLAUDE.md with test details
