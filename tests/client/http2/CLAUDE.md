# HTTP/2 Client E2E Tests

## Overview

These tests verify HTTP/2 client functionality using black-box testing with the actual NetGet binary.

## Test Strategy

### Approach

- **Black-box testing**: Spawn NetGet process, send instructions, verify output
- **Client-Server pairs**: Start HTTP/2 server, then HTTP/2 client
- **LLM-driven**: Tests rely on LLM understanding HTTP/2 protocol
- **Minimal LLM calls**: < 10 calls per test suite

### Test Scenarios

1. **Basic GET Request**: Client makes simple GET request to HTTP/2 server
2. **LLM-Controlled Request**: Verify LLM can generate custom HTTP/2 requests
3. **Multiplexing**: Test concurrent requests on single connection (HTTP/2 feature)

## LLM Call Budget

| Test                                       | LLM Calls | Rationale                                  |
|--------------------------------------------|-----------|--------------------------------------------|
| `test_http2_client_get_request`            | 2         | Server startup (1) + client connection (1) |
| `test_http2_client_llm_controlled_request` | 2         | Server startup (1) + client connection (1) |
| `test_http2_client_multiplexing`           | 2         | Server startup (1) + client connection (1) |
| **Total**                                  | **6**     | Well under 10-call budget                  |

## Expected Runtime

- **Individual test**: 2-3 seconds
- **Full suite**: 6-10 seconds
- **With LLM**: +2-5 seconds per LLM call (depends on model/load)

**Total estimated runtime**: 15-30 seconds

## Test Details

### Test 1: Basic GET Request

**Objective**: Verify HTTP/2 client can make a simple GET request

**Setup**:

1. Start HTTP/2 server on available port
2. Start HTTP/2 client connecting to server

**Verification**:

- Client output contains "HTTP2" or "http2" or "HTTP/2" or "connected"

**LLM Calls**: 2

### Test 2: LLM-Controlled Request

**Objective**: Verify LLM can understand and generate HTTP/2 requests

**Setup**:

1. Start HTTP/2 server
2. Start HTTP/2 client with instruction to send custom headers

**Verification**:

- Client protocol is "HTTP2"

**LLM Calls**: 2

### Test 3: Multiplexing

**Objective**: Demonstrate HTTP/2 stream multiplexing capability

**Setup**:

1. Start HTTP/2 server
2. Start HTTP/2 client with instruction to make multiple requests

**Verification**:

- Client output shows HTTP/2 protocol usage

**LLM Calls**: 2

**Note**: This test demonstrates the multiplexing instruction to LLM, but reqwest handles actual multiplexing
transparently.

## Known Issues

### Issue 1: HTTP/2 vs HTTP/1.1 Negotiation

**Problem**: reqwest may fall back to HTTP/1.1 if server doesn't advertise HTTP/2

**Mitigation**: Use `http2_prior_knowledge()` to force HTTP/2 protocol

**Test Impact**: None (implementation uses `http2_prior_knowledge()`)

### Issue 2: Cleartext HTTP/2 (h2c)

**Problem**: Some servers require TLS for HTTP/2

**Mitigation**: Tests use localhost with `http2_prior_knowledge()` for cleartext h2c

**Test Impact**: Tests work with cleartext HTTP/2 (h2c)

### Issue 3: Server Push Not Testable

**Problem**: reqwest API doesn't expose server push

**Mitigation**: Document limitation, skip server push testing

**Test Impact**: No server push tests

## Test Execution

### Run All HTTP/2 Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features http2 --test client::http2::e2e_test
```

### Run Single Test

```bash
./cargo-isolated.sh test --no-default-features --features http2 test_http2_client_get_request
```

### Prerequisites

- HTTP/2 server implementation (NetGet http2 server protocol)
- Ollama running with model loaded

## Success Criteria

- All tests pass
- Total LLM calls < 10
- Runtime < 60 seconds
- No flaky tests (99%+ pass rate)

## Future Enhancements

1. **HTTPS/TLS Testing**: Test HTTPS with ALPN negotiation
2. **Server Push**: If reqwest exposes API, test server push
3. **Stream Priority**: Test stream weight/priority if exposed
4. **Error Scenarios**: Test 404, 500, connection errors
5. **Large Payloads**: Test streaming large request/response bodies

## References

- Parent implementation: `src/client/http2/CLAUDE.md`
- Test helpers: `tests/helpers/client.rs`, `tests/helpers/common.rs`
- HTTP/2 spec: RFC 7540

## `command_channel_test.rs`

Covers `AppState::send_to_client` injecting an action into a running http2 client (the
dashboard's `[ send ]`). **Zero LLM calls**: the client's LLM points at
`http://127.0.0.1:1`, so its connected-event call fails and the loop must tolerate that —
part of what the test verifies. It always `wait_for_client_handle`s before sending, which is
the regression guard for "register the command channel *before* the connected-event LLM
call"; register it after and a client whose connect event parks on a manual rule reads "no
command channel" for the whole park.

Asserts the exact `ClientSendOutcome` variant. A successful request is
`Executed { detail }`, **not** `Sent` — reqwest/h3 own the socket and report no wire byte
count, so a byte count would be invented; the detail carries what actually came back
instead. An unknown action must be `Rejected` (not silently swallowed), and `disconnect`
must be `Disconnected` and leave the client with no command handle.

The peer is a NetGet HTTP/2 server of our own, started without `tls_enabled` so it speaks
cleartext h2c — which is what the client's `http2_prior_knowledge()` requires. A `*` static
handler answers, so the `200` and the server's access-log entry for the path are a real round
trip.
