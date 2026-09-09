# ICMP Server E2E Test Strategy

## Overview

Testing the ICMP server presents unique challenges due to raw socket requirements and privilege constraints.

## Privilege Requirements

**CRITICAL**: ICMP server tests require `CAP_NET_RAW` or root access.

- Tests are marked with `#[ignore]` by default
- Cannot run in unprivileged environments (including Claude Code for Web)
- Must explicitly enable with `cargo test -- --ignored --test-threads=100`

`test_icmp_echo_server` was **not** `#[ignore]`d until August 2026. It checked for raw-socket
capability, printed "⚠ Skipping" and returned `Ok(())`, which cargo reported as a **pass** on
every unprivileged machine - so the suite looked like it covered ICMP and covered nothing. It is
now `#[ignore]`d with a reason (cargo prints *ignored*, which nobody mistakes for a pass), and
running it explicitly with `--ignored` on a host that still lacks the privilege **fails loudly**
rather than skipping: you asked for the privileged test.

## What runs unprivileged, and is therefore the real coverage

`packet_codec_test.rs` is the file to read first. It needs no socket, no interface and no
privilege, because `IcmpServer::build_echo_reply` / `build_destination_unreachable` /
`build_time_exceeded` are pure functions and `IcmpProtocol::execute_action` is a pure
`Value -> ActionResult` mapping. It asserts:

| Test | What it pins |
|---|---|
| `echo_reply_matches_the_rfc_792_layout` | every field of the reply at its RFC 791/792 offset, both checksums *verified* by ones' complement sum rather than compared to a constant |
| `echo_reply_with_no_payload_is_still_a_whole_message` | a zero-length ping is legal and comes out 28 bytes |
| `destination_unreachable_matches_the_rfc_792_layout` | type 3, the code asked for, the four mandatory zero bytes, the datagram quoted verbatim |
| `a_longer_original_datagram_is_quoted_at_28_bytes` | the RFC 792 truncation |
| `a_shorter_original_datagram_does_not_panic` | a three-byte quotation; `[..28]` here would kill the receive task |
| `time_exceeded_matches_the_rfc_792_layout`, `time_exceeded_defaults_to_ttl_exceeded_in_transit` | type 11 and the documented default code |
| `raw_send_fixup_matches_the_platform`, `raw_send_fixup_ignores_a_runt` | what `prepare_ipv4_for_raw_send` does for the platform being compiled for |
| `every_advertised_example_is_accepted_by_its_own_executor` | the shape the model copies |
| `the_echo_reply_example_produces_the_packet_it_describes` | `payload_hex` is decoded, not sent as text |
| `ignore_icmp_puts_nothing_on_the_wire` | `NoAction`, not an error |
| `malformed_actions_are_refused_rather_than_panicking` | thirteen hostile shapes: non-hex, odd-length hex, out-of-range identifier, a stringified number, an IPv6 address, a payload larger than a datagram |

Assert against offsets, not golden blobs: a failure then names the field that moved.

**What it cannot prove**: that any of these bytes reach a wire. Everything past `send_to` needs
root, and no test in this repo has run there. Do not let a green `packet_codec_test` be read as
"ICMP works".

## Startup-failure coverage that does run unprivileged

`tests/capture_startup_reports_failure_test.rs::icmp_spawn_outcome_matches_raw_socket_privilege`
asserts that `IcmpServer::spawn_with_llm` returns `Ok` **iff** the raw sockets actually opened,
and that the unprivileged refusal names the socket and how to get it. ICMP once returned `Ok`
regardless, leaving a server in `ServerStatus::Running` that had received nothing; this is the
guard against that returning.

## Test Approach

### Option 1: Mock-Based Testing (Recommended)
Use `.with_mock()` pattern to verify LLM integration without raw sockets:

```rust
#[tokio::test]
async fn test_icmp_echo_request_with_mocks() {
    let config = NetGetConfig::new("Listen for ICMP echo requests")
        .with_mock(|mock| {
            mock
                .on_event("icmp_echo_request")
                .and_event_data_contains("source_ip", "127.0.0.1")
                .respond_with_actions_from_event(|event_data| {
                    let identifier = event_data["identifier"].as_u64().unwrap();
                    let sequence = event_data["sequence"].as_u64().unwrap();
                    let payload_hex = event_data["payload_hex"].as_str().unwrap();

                    serde_json::json!([{
                        "type": "send_echo_reply",
                        "source_ip": "127.0.0.1",
                        "destination_ip": "127.0.0.1",
                        "identifier": identifier,
                        "sequence": sequence,
                        "payload_hex": payload_hex
                    }])
                })
                .expect_calls(1)
                .and()
        });

    // Mock mode testing verifies action generation without raw sockets
    let server = config.start_server("icmp", "127.0.0.1:0", Some(json!({"interface": "eth0"}))).await?;

    // Simulate echo request event
    // ... test logic ...

    server.verify_mocks().await?;
}
```

### Option 2: Real Socket Testing (Privileged)
Requires root/CAP_NET_RAW:

```rust
#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored --test-threads=100
async fn test_icmp_echo_real_socket() {
    // Check for privileges
    if !has_raw_socket_capability() {
        eprintln!("Skipping test - requires CAP_NET_RAW or root");
        return;
    }

    // Start ICMP server with real socket
    // Send ping using pnet or external ping tool
    // Verify reply
}
```

## Test Scenarios

### Scenario 1: Echo Request → Echo Reply
- **Input**: ICMP Echo Request (type 8)
- **LLM Action**: `send_echo_reply`
- **Verification**: Reply has matching identifier, sequence, payload

### Scenario 2: Timestamp Request → nothing
- **Input**: ICMP Timestamp Request (type 13)
- **LLM Action**: none exists. `send_timestamp_reply` is commented out in the source because
  pnet 0.35 has no timestamp packet types, so a type 13 arrives as a generic
  `icmp_other_message` and there is no action that can answer it. This scenario used to be
  listed as if it were implemented.

### Scenario 3: Ignore Packet
- **Input**: ICMP Echo Request
- **LLM Action**: `ignore_icmp`
- **Verification**: No reply sent

### Scenario 4: Destination Unreachable
- **Input**: Generic ICMP message
- **LLM Action**: `send_destination_unreachable`
- **Verification**: Unreachable message sent with correct code

## LLM Call Budget

The unprivileged tests make **zero** LLM calls: they are pure-function and pure-executor tests.
The `#[ignore]`d `test_icmp_echo_server` budgets 3 (one startup instruction, two echo
requests), which is what its `expect_calls(1)` rules describe.

## Runtime

Expected: < 30 seconds for full suite (with mocks)
- Mock tests: instant (no real network I/O)
- Real socket tests: ~5-10 seconds (with --ignored flag)

## Two bugs this suite carried, both of which made it pass while proving nothing

1. **The mock answered with `dest_ip`.** No ICMP action has that parameter; every reply was
   refused with "Missing 'destination_ip' parameter", and the test still returned `Ok(())`
   because a missing reply was optional (below). Check the field names against the protocol,
   not against the neighbouring suite.
2. **Its own sender had the bug it exists to catch.** `build_icmp_echo_request` produces a
   whole IPv4 packet and the socket did not set `IP_HDRINCL`, so the kernel prepended a header
   and netget received IP-in-IP — which is where the server's "IP-in-IP encapsulation on
   loopback" workaround came from. It has nothing to do with loopback.

Both are fixed, and the test now **fails** when no echo reply arrives rather than printing a
warning and returning `Ok(())`.

## Known Issues

1. **Kernel ICMP Handling**: Linux kernel may intercept Echo Requests
   - Workaround: Use non-standard ICMP types for testing
   - Alternative: Disable kernel ICMP with `sysctl -w net.ipv4.icmp_echo_ignore_all=1`

2. **Firewall Interference**: Firewall rules may block ICMP
   - Ensure loopback ICMP is allowed
   - Use `iptables -L` to check rules

3. **Privilege Escalation**: Cannot elevate privileges in test
   - Must run entire test suite with appropriate permissions
   - Use `sudo -E cargo test` or `setcap cap_net_raw+ep`

## Future Enhancements

1. **Privilege Detection**: Auto-skip tests without CAP_NET_RAW
2. **Mock Packet Injection**: Simulate raw socket reads without kernel
3. **Docker Test Container**: Isolated environment with raw socket access
4. **IPv6 Testing**: ICMPv6 echo request/reply

## References

- RFC 792 - ICMP specification
- Similar tests: `tests/server/arp/e2e_test.rs` (also requires raw sockets)
- Mock pattern: `tests/server/dns/e2e_test.rs`
