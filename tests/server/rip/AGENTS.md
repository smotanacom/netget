# RIP E2E Test Documentation

## Overview

End-to-end tests for RIP (Routing Information Protocol) server implementation, validating protocol compliance and
LLM-controlled routing decisions.

## Test Strategy

### Black-Box Testing

Tests interact with NetGet via UDP (RIP uses UDP port 520):

- Send RIP request messages
- Receive RIP response messages
- Validate message format and route entries
- No access to internal server state

### LLM Call Efficiency

**Target**: < 10 LLM calls per test suite

**Actual LLM calls** (all against `tests/helpers/mock_ollama.rs`; no Ollama anywhere):

| suite | calls |
|---|---|
| `e2e_test::test_rip_routing_table_request` | 1 startup + 1 `rip_request` |
| `e2e_test::test_rip_route_advertisement` | 1 startup + 1 `rip_request` |
| `e2e_test::test_rip_metric_handling` | 1 startup + 1 `rip_request` |
| `static_default_test` | 1 startup, and `expect_calls(0)` on `rip_request` — the assertion *is* the zero |
| `action_validation_test` | 0 — it calls `execute_action` directly, no server |

**Total**: 7 (well under budget)

### Test Organization

Each test creates a new server instance with a specific prompt describing routing behavior. This allows testing
different routing scenarios without requiring complex multi-step interactions.

## Test Cases

### 1. test_rip_routing_table_request

**Purpose**: Verify server responds to routing table requests with advertised routes

**LLM Prompt**:

```
listen on port 0 via rip.
When you receive a RIP request for the entire routing table (AFI=0, metric=16),
respond with routes for:
- 192.168.1.0/24 with metric 1 and next hop 0.0.0.0
- 10.0.0.0/8 with metric 5 and next hop 0.0.0.0
- 172.16.0.0/12 with metric 3 and next hop 192.168.1.1
```

**Test Flow**:

1. Client sends RIP request (AFI=0, metric=16 = entire table)
2. Server responds with RIP response containing routes
3. Validate response format (command=2, version=2)
4. Verify at least 2 routes returned
5. Check for expected route (192.168.x.x with metric ≤ 1)

**LLM Calls**: 1 (server startup only)

**Expected Runtime**: ~120 seconds (includes LLM response time)

### 2. test_rip_route_advertisement

**Purpose**: Verify server advertises specific routes with correct format

**LLM Prompt**:

```
listen on port 0 via rip.
For any RIP request, advertise the following routes:
- 10.20.30.0/24 with metric 1
- 172.30.0.0/16 with metric 8
```

**Test Flow**:

1. Client sends RIP request
2. Server responds with advertised routes
3. Validate each route has valid metric (1-16)
4. Verify AFI=2 (IPv4)
5. Check route format compliance

**LLM Calls**: 1 (server startup only)

**Expected Runtime**: ~120 seconds

### 3. test_rip_metric_handling

**Purpose**: Verify server handles different metric values correctly

**LLM Prompt**:

```
listen on port 0 via rip.
Advertise routes with different metrics:
- 192.168.100.0/24 with metric 1 (directly connected)
- 10.10.0.0/16 with metric 5 (5 hops away)
- 172.20.0.0/16 with metric 15 (15 hops away, maximum reachable)
- 192.168.99.0/24 with metric 16 (unreachable/withdrawn)
```

**Test Flow**:

1. Client sends RIP request
2. Server responds with routes having various metrics
3. Verify presence of routes with different metric ranges:
    - Low (1-3): Directly connected
    - Medium (4-10): Multi-hop reachable
    - High (11-15): Maximum reachable
    - Infinity (16): Unreachable

**LLM Calls**: 1 (server startup only)

**Expected Runtime**: ~120 seconds

## RIP Protocol Compliance

Tests validate:

- **Message Format**: 4-byte header (command, version, unused)
- **Route Entry Format**: 20 bytes per entry (AFI, tag, IP, mask, next hop, metric)
- **Version**: RIPv2 (version field = 2)
- **Command Types**: Request (1), Response (2)
- **Metric Range**: 1-15 reachable, 16 = infinity (unreachable)
- **AFI**: IPv4 = 2

## Known Limitations

### Protocol Limitations

1. **No Periodic Updates**: Server only responds to requests (no 30-second timer)
2. **No Route Learning**: Server doesn't learn routes from other routers
3. **No Loop Prevention**: No split horizon, poison reverse, or hold-down timers
4. **No Authentication**: RIP MD5 authentication (RFC 2082) not implemented

### Test Limitations

0. **No real RIP implementation has ever been pointed at this server.** Every "client" in these
   suites is a RIP datagram assembled by hand from RFC 2453 inside the test — an independent
   reading of the spec, not an independent implementation. That is the `dhcp` situation, and it
   is why RIP stays Experimental. Driving it with FRRouting's `ripd` is what promotion needs.
1. **Single-Server Testing**: Tests don't verify multi-router convergence
2. **No Update Timers**: Can't test periodic update behavior
3. **No Route Poisoning**: Can't test triggered updates or route withdrawal timing
4. **No Large Tables**: Tests use small routing tables (< 25 routes)

## Running Tests

### Prerequisites

None beyond a build. **No Ollama is required or used** — every suite here runs against
`MockOllamaServer`, an in-process axum server, and binds only to localhost UDP ports. (This
section used to demand `ollama serve` while a later section in the same file correctly said the
model is mocked in-process; the mock is the truth.)

### Run Command

`--test` names a *cargo test target*, not a module path. The target is `server`; the module
path is a filter that goes after `--`:

```bash
# Every RIP server suite
./cargo-isolated.sh test --no-default-features --features rip --test server -- rip --test-threads=100

# One suite, with output
./cargo-isolated.sh test --no-default-features --features rip --test server -- rip::e2e_test --nocapture
```

### Expected Output

```
=== Test: RIP Routing Table Request ===
  [TEST] Creating UDP socket for RIP client
  [TEST] Sending RIP request to 127.0.0.1:52000
  [TEST] Waiting for RIP response
  [TEST] Received 84 bytes
  [TEST] RIP response: command=2, version=2, routes=3
  [TEST] Route: 192.168.1.0/255.255.255.0 via 0.0.0.0 metric 1
  [TEST] Route: 10.0.0.0/255.0.0.0 via 0.0.0.0 metric 5
  [TEST] Route: 172.16.0.0/255.240.0.0 via 192.168.1.1 metric 3
  [TEST] ✓ RIP routing table request test passed

=== Test: RIP Route Advertisement ===
  ...
  [TEST] ✓ RIP route advertisement test passed

=== Test: RIP Metric Handling ===
  ...
  [TEST] ✓ RIP metric handling test passed

test result: ok. 3 passed; 0 failed
```

## Performance Characteristics

### Runtime Breakdown

**Per test**: process spawn and server startup dominate; the mocked model answers immediately,
so a suite is seconds rather than minutes. The 60-90s-per-LLM-call figures this section used to
quote were measured against a real model and have not applied since the suites were mocked.

**Parallel execution**: supported, and `--test-threads=100` is the expected way to run. The
"not supported (Ollama lock)" note was wrong on both halves — there is no shared model server,
and `--ollama-lock` is an accepted no-op that serialises nothing
(`tests/ollama_lock_is_a_noop_test.rs`).

### Resource Usage

- **Memory**: ~50-100 MB per server instance
- **CPU**: Minimal (mostly waiting for LLM)
- **Network**: Localhost only, < 1 KB per test
- **Ollama**: mocked in-process; no shared model server is involved

## Debugging Failed Tests

### Common Failures

1. **Timeout waiting for response**
    - Read what `verify_mocks()` reports: it names the rule that fell short, which is usually a
      mock keyed on an event id or field the server does not actually raise
    - Confirm the rule is on `rip_request` — that is the only event this server emits
    - Check `decision=` in the log: `static_default_silent` means the server answered with the
      no-policy default and never called the model at all

2. **Invalid RIP message format**
    - Check server logs in test output
    - Verify LLM generated correct actions
    - May need to refine prompt

3. **Wrong route count**
    - LLM may have interpreted prompt differently
    - Check actual routes returned in test output
    - Adjust expectations or prompt clarity

4. **Port binding errors**
    - Another test may still be running
    - Use `lsof -i :520` to check for conflicts
    - Wait a few seconds and retry

### Useful Debugging Commands

```bash
# View test output with tracing
RUST_LOG=debug ./cargo-isolated.sh test --features rip --test rip::e2e_test -- --nocapture

# Run single test
./cargo-isolated.sh test --features rip --test rip::e2e_test -- test_rip_routing_table_request --nocapture

# Check server logs (written to working directory)
tail -f netget.log
```

## Test Maintenance

### When to Update Tests

- **Protocol changes**: If RIP message format changes
- **Action changes**: If RIP actions are added/removed/modified
- **Prompt sensitivity**: If LLM behavior changes significantly

### Test Stability

Tests are designed to be:

- **Deterministic**: Same prompt should yield consistent behavior
- **Isolated**: Each test runs independent server
- **Forgiving**: Tests check essential behavior, not exact values
- **Fast**: < 10 LLM calls total

## References

- [RFC 2453: RIPv2](https://datatracker.ietf.org/doc/html/rfc2453)
- [RFC 1058: RIPv1](https://datatracker.ietf.org/doc/html/rfc1058)
- RIP Packet Format: 4-byte header + N×20-byte route entries
- Maximum 25 routes per packet (504-byte maximum)
