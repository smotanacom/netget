# HTTP/3 Client E2E Tests

## Overview

**Every test in `e2e_test.rs` is `#[ignore]`d and none of them has ever run.** They are
written against a "NetGet HTTP/3 server" that does not exist: the `http3` feature builds
the *client* only, `base_stack: HTTP3` cannot start anything, and `src/server/quic/` is a
raw-QUIC server under ALPN `h3` with no RFC 9114 framing — no HTTP/3 client, this one
included, can talk to it. Root `CLAUDE.md` also forbids reaching an external endpoint, so
there is nothing on the machine for the client to connect to.

The only coverage that executes is `command_channel_test.rs`, which injects actions into a
running client and never opens a QUIC connection. **Nothing here has ever asserted this
client against a real HTTP/3 server**, and `metadata().e2e_testing` now says so.

Read the rest of this file as a description of what the ignored tests *would* check if an
HTTP/3 server protocol existed — the plan for one is in `src/server/quic/CLAUDE.md` under
"If a real HTTP/3 server is wanted". Do not read it as evidence.

## Test Strategy

### Approach: Black-Box Testing

Tests spawn actual NetGet processes as black boxes:

1. Start NetGet HTTP/3 server
2. Start NetGet HTTP/3 client with instruction
3. Verify client behavior via output
4. Verify server received request (via logs)

### Test Infrastructure

- **Server**: NetGet HTTP/3 server (built-in)
- **Client**: NetGet HTTP/3 client
- **Transport**: QUIC over UDP (localhost)
- **Verification**: Process output inspection

## Test Cases

### 1. `test_http3_client_get_request`

**Purpose**: Verify basic GET request over QUIC

**Flow**:

1. Start HTTP/3 server on available port
2. Start HTTP/3 client with GET instruction
3. Verify client output shows HTTP/3/QUIC connection
4. Clean up both processes

**LLM Calls**: 2 (server startup, client connection)

**Expected Runtime**: ~3-4 seconds

- Server startup: 1s
- Client connection + QUIC handshake: 2s
- Verification: <1s

**Assertions**:

- Client output contains "HTTP/3", "HTTP3", "QUIC", or "connected"

### 2. `test_http3_client_with_priority`

**Purpose**: Verify stream priority control.

Note that this test asks for `"priority": 7` as "high priority". Under RFC 9218 — which is
what the client now sends, as a `priority: u=N` header — **7 is the least urgent value**
and 0 the most. The test asserts nothing about the priority reaching the wire, so it would
still pass; the number is simply misleading and should be `1` if this test is ever
un-ignored.

**Flow**:

1. Start HTTP/3 server configured to log stream priorities
2. Start client with high-priority request instruction
3. Verify client protocol is HTTP3
4. Clean up

**LLM Calls**: 2 (server startup, client connection)

**Expected Runtime**: ~3-4 seconds

**Assertions**:

- Client protocol is "HTTP3"

**Note**: Server-side priority verification not yet implemented (would require log parsing)

### 3. `test_http3_client_llm_controlled`

**Purpose**: Verify LLM can control request details (method, headers, body)

**Flow**:

1. Start HTTP/3 server that echoes POST bodies
2. Start client with POST + JSON body instruction
3. Verify HTTP/3/QUIC transport used
4. Clean up

**LLM Calls**: 2 (server startup, client connection)

**Expected Runtime**: ~3-4 seconds

**Assertions**:

- Client output shows HTTP3 or QUIC usage

## LLM Call Budget

**Total**: 6 LLM calls across 3 tests

| Test                             | Server Startup | Client Action | Total |
|----------------------------------|----------------|---------------|-------|
| test_http3_client_get_request    | 1              | 1             | 2     |
| test_http3_client_with_priority  | 1              | 1             | 2     |
| test_http3_client_llm_controlled | 1              | 1             | 2     |

**Justification**: Minimal LLM usage while covering key scenarios:

- Basic GET (connectivity)
- Stream priorities (QUIC feature)
- LLM control (POST with body)

## Test Execution

### Running Tests

```bash
# All HTTP/3 client tests
./cargo-isolated.sh test --no-default-features --features http3 --test client::http3::e2e_test

# Specific test
./cargo-isolated.sh test --no-default-features --features http3 test_http3_client_get_request
```

### Prerequisites

- **HTTP/3 server support**: NetGet must be compiled with `http3` feature
- **QUIC transport**: UDP connectivity on localhost
- **No firewall blocking**: UDP port must be accessible

## Known Issues

### 1. QUIC Connection Timeout

**Issue**: QUIC handshake may timeout on slow systems

**Workaround**: Increased sleep durations (2s instead of 500ms)

**Future**: Implement retry logic or connection timeout detection

### 2. Stream ID Not Verified

**Issue**: Tests don't verify actual stream IDs

**Reason**: Stream IDs not exposed by h3/quinn easily

**Future**: Add stream ID tracking in implementation

### 3. TLS Certificate Verification Disabled

**Issue**: Tests use self-signed certs with verification disabled

**Impact**: Doesn't test real-world TLS scenarios

**Future**: Generate valid test certificates or use CA-signed test certs

### 4. No External Server Tests

**Issue**: All tests use NetGet's own HTTP/3 server

**Limitation**: Doesn't verify interoperability with other implementations

**Not a future option**: tests must bind to localhost only and never contact external
endpoints (root `CLAUDE.md`). Reaching Cloudflare or Google would also make the suite
depend on someone else's uptime. The unblocking work is an HTTP/3 server protocol.

## Performance Considerations

### Test Speed

- **Slower than HTTP/1.1**: QUIC handshake takes longer than TCP
- **Per-request connections**: Each test creates new QUIC connection
- **TUI startup overhead**: Each NetGet instance has initialization cost

### Optimization Opportunities

1. **Connection reuse**: Keep QUIC connection alive between tests
2. **Parallel execution**: Run independent tests concurrently
3. **Mock QUIC**: Use mock QUIC transport for unit tests

## Comparison with HTTP/1.1 Tests

| Aspect              | HTTP/1.1 Tests  | HTTP/3 Tests             |
|---------------------|-----------------|--------------------------|
| **Transport**       | TCP (localhost) | QUIC/UDP (localhost)     |
| **Handshake**       | Fast (~10ms)    | Slower (~100ms)          |
| **Server**          | HTTP server     | HTTP/3 server            |
| **Features Tested** | Basic requests  | Priorities, multiplexing |
| **Runtime**         | ~1-2s per test  | ~3-4s per test           |
| **Complexity**      | Low             | Medium                   |

## Future Enhancements

### High Priority

1. **Stream ID Verification**
    - Verify server assigns correct stream IDs
    - Test multiplexing (concurrent streams)

2. **0-RTT Testing**
    - Test connection resumption
    - Verify session ticket reuse

3. **Connection Migration**
    - Simulate IP address change
    - Verify QUIC handles migration

### Medium Priority

4. **External Server Tests**
    - Test against a local HTTP/3 server, once one exists (never an external endpoint)
    - Verify interoperability

5. **Error Scenarios**
    - Server unavailable
    - QUIC connection refused
    - TLS handshake failure

6. **Performance Tests**
    - Measure latency vs HTTP/1.1
    - Test multiplexing throughput

### Low Priority

7. **Advanced QUIC Features**
    - Connection migration
    - Flow control
    - Congestion control

8. **WebTransport Tests**
    - If/when WebTransport support added

## Debugging Tips

### Enable Verbose Logging

Set `RUST_LOG=debug` to see QUIC/HTTP/3 internals:

```bash
RUST_LOG=debug ./cargo-isolated.sh test --features http3 test_http3_client_get_request
```

### Check QUIC Connection

Verify QUIC handshake in logs:

- Look for "QUIC connection established"
- Check for TLS handshake completion
- Verify HTTP/3 session creation

### Inspect Network Traffic

Use Wireshark to inspect QUIC packets:

```bash
# Capture UDP traffic on loopback
sudo tcpdump -i lo -w http3-test.pcap udp
```

### Common Failure Modes

1. **"Connection refused"**: HTTP/3 server not started or wrong port
2. **"TLS handshake failed"**: Certificate issues (usually skip verification)
3. **"Timeout"**: QUIC handshake took too long (increase sleep)
4. **"Protocol not supported"**: HTTP/3 feature not compiled in

## References

- **NetGet Testing Guide**: `/home/user/netget/tests/README.md`
- **QUIC Debugging**: https://github.com/quinn-rs/quinn/blob/main/docs/debugging.md
- **HTTP/3 RFC**: RFC 9114
- **Test Helpers**: `/home/user/netget/tests/helpers/client.rs`

## `command_channel_test.rs`

Covers `AppState::send_to_client` injecting an action into a running http3 client (the
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

There is **no wire assertion**, deliberately: the `http3` feature builds the client only, so
there is no NetGet HTTP/3 server to send a QUIC request to, and aiming the client at a closed
UDP port would assert a quinn handshake timeout rather than anything about this feature. What
is pinned down is the part the command channel owns — handle registered, `Rejected`,
`Disconnected`, access log.
