# IGMP Client E2E Test Documentation

## Test Strategy

**Approach**: Black-box prompt-driven testing using the NetGet binary
**LLM Budget**: < 10 total LLM calls across all tests
**Runtime**: ~10-15 seconds

**Nothing here is `#[ignore]`d or privilege-gated.** `cargo test --no-default-features
--features igmp --test client -- client::igmp` runs 4 tests and all 4 pass. That is the whole
difference from the *server* suite, whose four cases are ignored behind root — see
`tests/server/igmp/CLAUDE.md`.

## Test Organization

`tests/client/igmp/e2e_test.rs` (3 black-box tests through the real binary) and
`command_channel_test.rs` (1 in-process test of the dashboard's `[ send ]` path), both feature
gated `#[cfg(all(test, feature = "igmp"))]` and declared in `tests/client/igmp/mod.rs`.

### Test Cases

#### 1. `test_igmp_client_join_and_receive`

**Purpose**: Verify client can join a multicast group and receive data
**LLM Calls**: 2 (client startup, multicast join instruction)
**Flow**:

1. Start IGMP client with instruction to join group 239.255.1.1
2. Send UDP packet to multicast group from external sender
3. Verify client receives and logs the data
4. Cleanup

**Expected Runtime**: ~4 seconds

---

#### 2. `test_igmp_client_join_and_leave`

**Purpose**: Verify client can join and then leave a multicast group
**LLM Calls**: 3 (client startup, join instruction, leave instruction)
**Flow**:

1. Start IGMP client with instruction to join and leave group 239.255.1.2
2. Verify client processes both actions
3. Cleanup

**Expected Runtime**: ~4 seconds

**Weakest of the three, knowingly.** It asserts the mock exchange happened and that the client
is an IGMP client; it does not observe the membership. NetGet emits no bytes for a join — the
kernel does — so proving it would mean capturing IGMP off an interface, which needs privilege.
The honest coverage for the join *outcome* is `command_channel_test.rs`, which asserts
`Executed { detail: "joined …" }` rather than pretending a datagram was sent.

---

#### 3. `test_igmp_client_send_multicast`

**Purpose**: Verify client can send multicast data
**LLM Calls**: 2 (client startup, send instruction)
**Flow**:

1. Bind a loopback receiver and start the IGMP client aimed at it
2. **Wait for the four bytes to arrive**, and assert them
3. Cleanup

**Expected Runtime**: ~4 seconds

**This test used to assert nothing.** It mocked the connected event with a `"data"` field where
the action declares `"data_hex"`, so `execute_action` refused it with "Missing data_hex", no
datagram was ever sent, and the surviving assertions — `client.protocol == "igmp"` and the mock
call counts — were satisfied by the *request* rather than by anything the client did with the
answer. It passed for exactly as long as it proved nothing. Two lessons: **check the action's
declared parameter names against `actions.rs`, not against what reads naturally**, and **assert
on the wire, not on the fact that a mock was called**.

The destination is loopback rather than a real group, because `send_multicast` is a plain
`send_to` on the client's own socket: this exercises the whole path, while a real group would
make the result depend on the host having a multicast-capable interface and on IGMP snooping.

---

## LLM Call Budget

| Test                                | LLM Calls | Purpose                |
|-------------------------------------|-----------|------------------------|
| `test_igmp_client_join_and_receive` | 2         | Startup + join         |
| `test_igmp_client_join_and_leave`   | 3         | Startup + join + leave |
| `test_igmp_client_send_multicast`   | 2         | Startup + send         |
| **Total**                           | **7**     | Well under 10 budget   |

## Test Execution

### Running Tests

`--test` names a *target* (`client`), not a path; the module path is a filter argument.

```bash
./cargo-isolated.sh test --no-default-features --features igmp \
    --test client -- client::igmp --test-threads=100

# With output
./cargo-isolated.sh test --no-default-features --features igmp \
    --test client -- client::igmp --test-threads=100 --nocapture
```

### Prerequisites

1. **Ollama**: not needed. The mock runs in-process (`tests/helpers/mock_ollama.rs`), and
   `command_channel_test.rs` deliberately points at an unreachable URL so its connected-event
   call fails immediately — part of what it verifies is that `connect_with_llm_actions`
   tolerates that
2. **Network**: Multicast must work on localhost (default on most systems)
3. **Firewall**: Multicast traffic (239.0.0.0/8) not blocked

## Test Environment

### Multicast Groups Used

All tests use administratively-scoped multicast addresses (239.255.0.0/16):

- `239.255.1.1` - Join and receive test, on an **allocated** port
- `239.255.1.2` - Join and leave test
- `239.255.1.3` - Send test, which now sends to an allocated loopback port

Ports are allocated, never fixed. A multicast socket sets `SO_REUSEPORT`, so a second listener
on a fixed port does not fail to bind — it silently competes for the datagrams, and the test
that loses reports that the client never received anything.

**Why 239.255.x.x?**

- Organization-local scope (RFC 2365)
- Won't leak outside local network
- No conflicts with well-known multicast groups

### Platform Considerations

#### Linux

- Multicast loopback enabled by default
- Works without special configuration
- May require `IP_MULTICAST_LOOP` set to 1 (default)

#### macOS

- Multicast loopback enabled by default
- Works without special configuration

#### Windows

- Multicast loopback enabled by default
- Firewall may need configuration to allow multicast

## Known Issues

### 1. Timing Sensitivity

- Tests use `tokio::time::sleep()` to allow multicast join propagation
- May need adjustment on slow systems
- **Mitigation**: Increase sleep durations if tests are flaky

### 2. Multicast Not Received on Some Networks

- Some network configurations block multicast
- Virtual network adapters (VPN, Docker) may interfere
- **Mitigation**: Test on physical interface, ensure loopback works

### 3. IGMP Snooping

- Layer 2 switches with IGMP snooping may delay multicast delivery
- Loopback tests bypass this issue
- **Mitigation**: Use loopback (127.0.0.1) for local testing

### 4. Port Conflicts

Fixed. All three tests allocate their ports; there is no `--test-threads=1` requirement, and
adding one would hide the `SO_REUSEPORT` problem rather than fix it.

## Test Efficiency

### Why < 10 LLM Calls?

Each LLM call takes ~1-2 seconds:

- 7 total calls × 1.5s = ~10.5 seconds for LLM processing
- Add client startup, network I/O, cleanup = ~15 seconds total runtime

### Optimizations

1. **No server tests**: IGMP client doesn't require a NetGet server
2. **Simple prompts**: Direct instructions minimize LLM processing time
3. **Minimal verification**: Tests check basic behavior, not exhaustive edge cases
4. **Shared cleanup**: Reuse client instances where possible

## Debugging

### Enable Trace Logging

```bash
RUST_LOG=trace ./cargo-isolated.sh test --no-default-features --features igmp --test client::igmp::e2e_test -- --nocapture
```

### Check Multicast Reception

Manually verify multicast works:

```bash
# Terminal 1: Start netget IGMP client
./target/debug/netget --client igmp --remote "igmp" --instruction "Join group 239.255.1.1 on port 15000"

# Terminal 2: Send test packet
echo "TEST" | nc -u 239.255.1.1 15000
```

### Common Errors

**"Address already in use"**

- Another test or process using port 15000/15001
- Solution: Use different ports or kill conflicting process

**"Network is unreachable"**

- Multicast routing not configured
- Solution: Ensure loopback interface supports multicast

**"No such device"**

- Interface doesn't exist
- Solution: Use `0.0.0.0` (any interface)

## Future Test Enhancements

1. **Verify Kernel IGMP Messages**: Capture IGMP reports/leaves with pcap
2. **Multi-Group Stress Test**: Join 100+ groups simultaneously
3. **IPv6 Multicast**: Test ff02::1 (all nodes) and other IPv6 groups
4. **Source-Specific Multicast (SSM)**: Test IGMPv3 source filtering
5. **TTL Testing**: Verify multicast TTL behavior
6. **Cross-Host Testing**: Test multicast between different machines

## References

- RFC 1112: Host Extensions for IP Multicasting
- RFC 2236: Internet Group Management Protocol, Version 2
- RFC 3376: Internet Group Management Protocol, Version 3
- RFC 4607: Source-Specific Multicast for IP
