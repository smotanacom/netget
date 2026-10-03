# ICMP Client E2E Test Strategy

## Overview

Testing the ICMP client presents unique challenges due to raw socket requirements and external server dependencies.

## Privilege Requirements

**CRITICAL**: ICMP client tests require `CAP_NET_RAW` or root access.

- Tests are marked with `#[ignore]` by default
- Cannot run in unprivileged environments (including Claude Code for Web)
- Must explicitly enable with `cargo test -- --ignored --test-threads=100`

## Test Approach

### Option 1: Action and codec testing (no privileges required)

This is where the real coverage is: `action_codec_test.rs`. `IcmpClient::build_echo_request` is
a pure function and `IcmpClientProtocol::execute_action` is a pure
`Value -> ClientActionResult` mapping, so both can be pinned with no socket:

| Test | What it pins |
|---|---|
| `the_echo_request_matches_the_rfc_792_layout` | every field at its RFC 791/792 offset, both checksums *verified* by ones' complement sum, the model's `ttl` reaching the header, source left `0.0.0.0` for the kernel to fill |
| `an_empty_payload_still_builds_a_whole_message` | a 28-byte request is legal |
| `every_advertised_example_is_accepted_by_its_own_executor` | the shape the model copies |
| `send_echo_request_normalises_its_parameters` | the documented defaults (1234 / 1 / 64) |
| `wait_for_more_and_disconnect_are_real_answers` | both map to their `ClientActionResult` |
| `malformed_actions_are_refused_rather_than_panicking` | eight shapes, including a **stringified identifier**, which used to be silently replaced by the default |
| `the_model_is_offered_a_vocabulary_on_every_event` | clients union async ∪ sync ∪ the event's list, so the sync list must not be empty |

There is **no** `send_timestamp_request` to assert — the action is commented out because pnet
0.35 has no timestamp packet types. This section previously asserted its existence.

### Option 2: Real Socket Testing (Privileged)

Requires root/CAP_NET_RAW:

```rust
#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored --test-threads=100
async fn test_icmp_echo_request() {
    // Check for privileges
    if !has_raw_socket_capability() {
        eprintln!("Skipping test - requires CAP_NET_RAW or root");
        return;
    }

    // Create ICMP client
    let client = create_test_client("Send echo request to 8.8.8.8").await?;

    // Wait for reply event
    // Verify RTT calculation
    // Verify LLM action generation
}
```

## Test Scenarios (privileged, none implemented)

**Event ids, verified against `src/client/icmp/actions.rs`**: `icmp_connected`,
`icmp_echo_reply`, `icmp_timeout`, `icmp_destination_unreachable`, `icmp_time_exceeded`. This
file used to name `icmp_echo_reply_received` and `icmp_timestamp_reply_received`, neither of
which exists — a mock written against them would never match, the event would fall through to
a real LLM call, and the failure would surface two steps later.

**Target 127.0.0.1, never a public address.** The repo forbids tests contacting external
endpoints; earlier revisions of this file specified 8.8.8.8.

### Scenario 1: Echo Request → Echo Reply (Ping)
- **Action**: `send_echo_request` to 127.0.0.1
- **Expected Event**: `icmp_echo_reply` with matching identifier/sequence
- **Verification**: RTT calculation, payload matching

### Scenario 2: Timeout
- **Action**: `send_echo_request` to an address on a discard route
- **Expected Event**: `icmp_timeout` after `ICMP_REPLY_TIMEOUT_SECS`, carrying `waited_ms`
- **Verification**: the pending entry is removed exactly once

### Scenario 3: Traceroute Simulation
- **Action**: Multiple `send_echo_request` with increasing TTL
- **Expected Events**: `icmp_time_exceeded` from intermediate hops, then `icmp_echo_reply`
- **Verification**: hop tracking, RTT per hop. Note this is the scenario that could not have
  worked before `IP_HDRINCL` was set — the kernel chose the TTL, so the model's value never
  reached the wire.

### Scenario 4: Destination Unreachable
- **Action**: `send_echo_request` to an unreachable IP
- **Expected Event**: `icmp_destination_unreachable`
- **Verification**: unreachable code

## LLM Call Budget

Zero, today: every test that runs makes no LLM call at all. `command_channel_test.rs` points
the client's LLM at `http://127.0.0.1:1` deliberately, and `action_codec_test.rs` never
constructs a client. Any privileged test added later should budget one call per event it
provokes and no more.

## Runtime

Expected: < 30 seconds for full suite

- Action definition tests: instant (no network I/O)
- Real socket tests: ~10-20 seconds (with --ignored flag)
  - 8.8.8.8 RTT: ~20-50ms
  - Localhost RTT: <1ms
  - Traceroute: ~500ms per hop

## Known Issues

1. **Kernel ICMP Echo Handling**: Linux kernel may intercept Echo Replies
   - Workaround: Use non-localhost destinations for echo tests
   - Alternative: Test with timestamp requests (less common, not intercepted)

2. **Firewall Interference**: Firewall rules may block ICMP
   - Ensure outbound ICMP is allowed
   - Use `iptables -L OUTPUT` to check rules

3. **Network Dependency**: Tests require network connectivity
   - Use localhost (127.0.0.1) where possible
   - Public IPs (8.8.8.8) may fail in isolated environments

4. **Privilege Escalation**: Cannot elevate privileges in test
   - Must run entire test suite with appropriate permissions
   - Use `sudo -E cargo test` or `setcap cap_net_raw+ep`

5. **Non-deterministic Timing**: RTT varies based on network conditions
   - Use tolerance ranges in assertions (e.g., RTT < 100ms)
   - Retry transient failures

## Test Isolation

- **Port Conflicts**: N/A (ICMP is connectionless, no ports)
- **Parallel Execution**: Safe with `--test-threads=100`
  - Each test uses unique identifier values
  - No shared state between tests

## Future Enhancements

1. **Privilege Detection**: Auto-skip tests without CAP_NET_RAW
2. **Mock Network Layer**: Simulate ICMP replies without kernel
3. **Docker Test Container**: Isolated environment with raw socket access
4. **IPv6 Testing**: ICMPv6 echo request/reply
5. **LLM Mock Mode**: Pre-programmed action responses for deterministic testing

## References

- RFC 792 - ICMP specification
- Similar tests: `tests/client/tcp/e2e_test.rs` (socket-based client)
- Privilege handling: `tests/server/arp/e2e_test.rs` (also requires raw sockets)
