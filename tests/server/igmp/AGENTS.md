# IGMP Server Test Documentation

## What actually runs

Two files, and the distinction matters more than anything else here:

| File | Runs unprivileged? | Count |
|---|---|---|
| `packet_codec_test.rs` | **yes, always** | 16 |
| `e2e_test.rs` | **no — every case is `#[ignore]`d behind root** | 4 |

Before `packet_codec_test.rs` existed, `cargo test --no-default-features --features igmp --test
server -- server::igmp` reported **`0 passed; 4 ignored`**: nothing about the IGMP server ran in
any ordinary test run, on any machine, ever. An earlier version of this file claimed the
opposite — "Test approach validates protocol logic without requiring root for test execution" —
which was wrong twice: the tests need root, *and* they would not pass with it (see the second
blocker below). Read every ✅ below as a claim about the code, never as a test result.

## `packet_codec_test.rs` — the unprivileged evidence

Pure functions only: no socket, no interface, no privilege, no LLM. It asserts field by field
against the literal RFC 2236 §2 layout rather than against a golden blob, so a failure names the
field that moved.

- **Emitted packets.** `send_membership_report` and `send_leave_group` through the real
  `execute_action`: length 8, the type byte, Max Response Time zeroed, the group at offsets 4-7,
  and the checksum folding to 0. A report and a leave for one group must differ in the type byte
  **and** the checksum — a builder that forgot to recompute after changing the type would pass
  every other assertion.
- **The checksum.** Every single-bit flip in all 8 octets must invalidate it (64 cases), and
  `igmp_checksum(&[])` must not underflow the `data.len() - 1` in its fold loop.
- **Destinations.** `response_destination` against RFC 2236 §9 / §2.9 and RFC 3376 §4.2.14,
  including the v1 and v3 report cases that used to fall back to the sender.
- **Hostile input.** `igmp_payload` over every illegal IHL (0..5 words, and an IHL longer than
  the packet), every non-IGMP protocol number, and every length below 20; `IgmpMessage::parse`
  over all 256 type bytes and every length below 8; and `checksum_valid` on a corrupted message
  that still parses.
- **The executor.** Every rejection path of all four group verbs — missing, unparseable, IPv6,
  numeric, `null`, unicast, `0.0.0.0`, `240.0.0.1`, `255.255.255.255` — must be an error rather
  than a panic, because a panic here happens inside a spawned task where `tokio::spawn` swallows
  it and the server goes on reporting `Running`.

## `e2e_test.rs` — root-gated, and additionally broken

All four cases are `#[ignore]`d. Two independent blockers, both of which have to be fixed before
un-ignoring them is worth anything:

1. **Raw-socket privilege.** `server_startup` refuses IGMP without root/`CAP_NET_RAW`
   (`PrivilegeRequirement::RawSockets`), netget starts no server, and the harness fails with
   "No servers or clients started".
2. **The test client blocks the runtime.** It uses the blocking `std::net::UdpSocket`, and
   `#[tokio::test]` runs each case on a *current-thread* runtime shared with the in-process
   mocked Ollama server. A blocking `recv_from` parks the only worker, the mock cannot answer,
   and the read times out for a reason that has nothing to do with IGMP. Port them to
   `tokio::net::UdpSocket` as `tests/server/sip/e2e_test.rs` did.

A third problem is in the design rather than the plumbing: the tests send IGMP **over UDP to a
port**, while the server reads a **raw IPPROTO_IGMP socket** that receives whole IP packets and
strips the header itself. Those two do not meet. Exercising the real receive path needs a raw
socket on the sending side too, i.e. root on both ends.

## Test Organization

Both files are declared in `tests/server/igmp/mod.rs` and feature-gated
`#[cfg(all(test, feature = "igmp"))]`.

## Test Cases

### 1. General Query Response (`test_igmp_general_query_response`)

**Purpose**: Verify server responds to general membership queries for joined groups

**Test Flow**:

1. Start server with instruction to join 239.255.255.250
2. Send IGMPv2 Membership Query with group address 0.0.0.0 (general query)
3. Verify server responds with Membership Report (type 0x16) for 239.255.255.250

**LLM Calls**: 1 (server startup)

**Expected Runtime**: ~5 seconds

**Validation**:

- Response message type is 0x16 (Membership Report)
- Group address in report is 239.255.255.250

### 2. Group-Specific Query (`test_igmp_group_specific_query`)

**Purpose**: Verify server only responds to queries for joined groups

**Test Flow**:

1. Start server with instruction to join 224.0.1.1 and 239.1.2.3
2. Send group-specific query for 224.0.1.1 (joined group)
3. Verify server responds with report
4. Send group-specific query for 225.0.0.1 (non-joined group)
5. Verify server doesn't respond or responds appropriately

**LLM Calls**: 1 (server startup)

**Expected Runtime**: ~7 seconds

**Validation**:

- Server responds to queries for joined groups
- Server ignores or handles queries for non-joined groups gracefully

### 3. Report from Peer (`test_igmp_report_from_peer`)

**Purpose**: Verify server accepts IGMP reports from other hosts

**Test Flow**:

1. Start server with instruction to join 224.1.1.1
2. Send Membership Report from "peer" for 224.1.1.1
3. Verify server accepts packet without errors

**LLM Calls**: 1 (server startup)

**Expected Runtime**: ~5 seconds

**Validation**:

- Server accepts peer reports
- No crashes or errors
- (Optional) Server may suppress own reports per IGMP spec

### 4. Multiple Groups (`test_igmp_multiple_groups`)

**Purpose**: Comprehensive test with multiple groups and general query

**Test Flow**:

1. Start server with instruction to join 224.0.0.251 (mDNS) and 239.255.255.250 (SSDP)
2. Send general membership query
3. Verify server sends at least one report

**LLM Calls**: 1 (server startup)

**Expected Runtime**: ~8 seconds

**Validation**:

- Receives at least 1 membership report
- Reports are for joined groups

## LLM Call Budget

**Total LLM Calls**: 4 (one per test)

**Budget Compliance**: ✓ Well under 10 calls

**Efficiency**: Each test reuses server instance for multiple operations within the test

## Test Infrastructure

### Packet Construction

Tests manually construct IGMPv2 packets:

- `build_igmp_query()` - Membership Query (type 0x11)
- `build_igmp_report()` - Membership Report (type 0x16)
- `calculate_checksum()` - RFC 1071 Internet Checksum

### Packet Format (8 bytes)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     Type      | Max Resp Time |           Checksum            |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         Group Address                         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

### Transport

**Implementation**: IGMP server uses raw IP sockets (IPPROTO_IGMP)

**Testing**: The `#[ignore]`d e2e tests use `std::net::UdpSocket` and send IGMP payloads to a
UDP port. The server does not read a UDP port — it reads a raw IPPROTO_IGMP socket that receives
whole IP packets — so this does **not** validate protocol logic without root; it validates
nothing at all until both ends are raw sockets. `packet_codec_test.rs` is what covers the
protocol logic unprivileged, by calling the pure functions directly.

**Production**: Server uses `libc::socket()` with SOCK_RAW and IPPROTO_IGMP

## Known Limitations

### 1. Test Transport

**Note**: the e2e tests use UDP sockets while the server uses raw IP sockets. These are not the
same transport, and the packets never meet — which is one reason the tests were never observed
to pass. Do not read the UDP transport as "the same test without root"; it is a different test
that exercises nothing on the server side.

**Production Deployment**: Server requires root or CAP_NET_RAW capability

### 2. Real Network Testing

**Limitation**: Tests use localhost loopback

**Reason**: Privacy and offline operation requirements

**Impact**: Doesn't test actual multicast routing on real networks

**For Production**: Test on real multicast-enabled network with routers sending queries

### 3. Report Suppression Timing

**Issue**: IGMPv2 includes random delay and report suppression

**Reason**: Tests use short timeouts for speed

**Impact**: May not fully test report suppression behavior

**Workaround**: Tests note that report suppression is optional behavior

## Running Tests

`--test` names a *target* (`server`), not a path; the module path is a filter argument.

### The tests that run (no privilege, no Ollama):

```bash
./cargo-isolated.sh test --no-default-features --features igmp \
    --test server -- server::igmp::packet_codec --test-threads=100
```

### Everything, including the ignored e2e cases:

```bash
sudo ./cargo-isolated.sh test --no-default-features --features igmp \
    --test server -- server::igmp --include-ignored --test-threads=100
```

Expect the e2e cases to fail even under `sudo` until the blocking-socket and UDP-vs-raw problems
above are fixed. The mocked Ollama runs in-process, so no real model is needed.

## Test Reliability

**Timeouts**:

- Server initialization: 3 seconds
- Response wait: 5 seconds (first attempt), 2 seconds (subsequent)
- Total per test: 5-8 seconds

**Failure Modes**:

- LLM doesn't understand IGMP protocol → Bad response format
- LLM doesn't join groups → No reports sent
- LLM responds to wrong queries → Assertion failures

**Retry Logic**: None currently. Tests fail fast on errors.

## Privacy & Offline

All tests use:

- Localhost only (127.0.0.1)
- No external connections
- Multicast groups are standard (mDNS, SSDP) or test addresses

Tests work completely offline after Ollama model is downloaded.

## Future Enhancements

1. **Raw Socket Tests** (when implemented):
   ```rust
   // Verify raw IP socket with IPPROTO_IGMP
   // Verify IP_ADD_MEMBERSHIP/IP_DROP_MEMBERSHIP
   ```

2. **IGMPv3 Tests**:
   ```rust
   // Test source filtering (INCLUDE/EXCLUDE modes)
   // Test IGMPv3 report format (type 0x22)
   ```

3. **Router Mode Tests** (if implemented):
   ```rust
   // Verify router sends periodic queries
   // Verify group-specific queries after leave
   ```

4. **Performance Tests**:
   ```rust
   // Test query response timing
   // Test report suppression with multiple hosts
   ```

## Debugging

### Enable trace logging:

```bash
RUST_LOG=trace ./cargo-isolated.sh test --features igmp -- test_igmp_general_query_response --nocapture
```

### Inspect packets:

```rust
println!("Packet hex: {}", hex::encode(&packet));
```

### Common issues:

- **No response**: LLM didn't join group or didn't understand query
- **Wrong group**: LLM joined different group than expected
- **Invalid packet**: Checksum error or malformed response
- **Timeout**: LLM took too long to respond

## References

- RFC 2236: Internet Group Management Protocol, Version 2
- RFC 3376: Internet Group Management Protocol, Version 3
- Implementation: `src/server/igmp/CLAUDE.md`
