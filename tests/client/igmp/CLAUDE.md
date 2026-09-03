# IGMP Client E2E Test Documentation

## Test Strategy

**Approach**: Black-box prompt-driven testing using the NetGet binary
**LLM Budget**: < 10 total LLM calls across all tests
**Runtime**: ~10-15 seconds

## Test Organization

All tests are in `tests/client/igmp/e2e_test.rs` with feature gate `#[cfg(all(test, feature = "igmp"))]`.

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

---

#### 3. `test_igmp_client_send_multicast`

**Purpose**: Verify client can send multicast data
**LLM Calls**: 2 (client startup, send instruction)
**Flow**:

1. Start IGMP client with instruction to send data to group 239.255.1.3
2. Verify client sends the multicast packet
3. Cleanup

**Expected Runtime**: ~4 seconds

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

```bash
# Run IGMP client tests only (recommended during development)
./cargo-isolated.sh test --no-default-features --features igmp --test client::igmp::e2e_test

# Run with output
./cargo-isolated.sh test --no-default-features --features igmp --test client::igmp::e2e_test -- --nocapture
```

### Prerequisites

1. **Ollama**: Must be running with model loaded
2. **Network**: Multicast must work on localhost (default on most systems)
3. **Firewall**: Multicast traffic (239.0.0.0/8) not blocked

## Test Environment

### Multicast Groups Used

All tests use administratively-scoped multicast addresses (239.255.0.0/16):

- `239.255.1.1:15000` - Join and receive test
- `239.255.1.2` - Join and leave test
- `239.255.1.3:15001` - Send test

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

- Tests use fixed ports (15000, 15001)
- Parallel test execution may cause conflicts
- **Mitigation**: Use `--test-threads=1` or dynamic port allocation

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
