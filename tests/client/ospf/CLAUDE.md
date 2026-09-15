# OSPF Client E2E Tests

## Overview

End-to-end tests for the OSPF client protocol implementation.

**Test approach**: Black-box testing via NetGet binary
**LLM budget**: < 5 calls per test suite
**Runtime**: ~5-10 seconds (if privileged), instant (if skipped)

## Special Requirements

### Root Privileges Required

**CRITICAL**: OSPF client uses raw IP sockets (protocol 89) which require root/CAP_NET_RAW privileges.

**Running tests**:

```bash
# With root (tests will run)
sudo -E ./cargo-isolated.sh test --no-default-features --features ospf

# Without root (tests will skip with warning)
./cargo-isolated.sh test --no-default-features --features ospf
```

**Why root is needed**:

- Raw IP socket creation (IP protocol 89)
- Multicast group membership (224.0.0.5)
- IP header construction/parsing
- Platform-specific raw socket API

### Platform Support

**Supported platforms**:

- Linux (with root or CAP_NET_RAW)
- macOS (with root)

**Unsupported platforms**:

- Windows (raw IP sockets require admin + different API)

## Test Strategy

### Test Categories

**1. Initialization Tests** (1 LLM call)

- Verify OSPF client can be created
- Check privilege error handling
- Validate socket creation

**2. Basic Protocol Tests** (2 LLM calls)

- Send Hello packet to multicast
- Verify packet construction
- Check LLM action execution

**3. Full E2E Tests** (4 LLM calls)

- OSPF server + client interaction
- Hello packet exchange
- Neighbor discovery
- Bidirectional communication

### LLM Call Budget

| Test                            | LLM Calls | Rationale                                |
|---------------------------------|-----------|------------------------------------------|
| test_ospf_client_initialization | 1         | Client startup only                      |
| test_ospf_client_send_hello     | 2         | Client startup + send action             |
| test_ospf_client_with_server    | 4         | Server + client startup + Hello exchange |
| **Total**                       | **7**     | Under 10 LLM call budget ✅               |

### Privilege Handling

Tests automatically skip when root privileges are unavailable:

```rust
fn has_root_privileges() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}
```

**Output when skipped**:

```
⚠️  Skipping test: OSPF requires root privileges
   Run with: sudo -E cargo test --no-default-features --features ospf
```

## Test Descriptions

### test_ospf_client_initialization

**Purpose**: Verify OSPF client can be initialized with proper configuration

**Steps**:

1. Start OSPF client on loopback (127.0.0.1)
2. Configure to monitor (no packet sending)
3. Verify initialization output

**Expected behavior**:

- Client connects successfully (with root)
- Output mentions "OSPF" or "connected"
- Socket joins multicast group

**Assertions**:

```rust
assert!(output.contains("OSPF") || output.contains("ospf"));
```

**LLM calls**: 1 (client startup)

---

### test_ospf_client_send_hello

**Purpose**: Verify OSPF client can send Hello packet to multicast group

**Steps**:

1. Start OSPF client with specific router_id
2. Instruct LLM to send Hello packet
3. Verify Hello packet construction and transmission

**Expected behavior**:

- Client sends Hello to 224.0.0.5
- Output shows "Hello" or OSPF activity
- Packet includes router_id and area_id

**Assertions**:

```rust
assert!(output.contains("Hello") || output.contains("OSPF"));
```

**LLM calls**: 2 (client startup + send action)

---

### test_ospf_client_with_server

**Purpose**: Full E2E test with OSPF server and client exchanging Hello packets

**Steps**:

1. Start OSPF server on interface A
2. Start OSPF client on interface B
3. Client sends Hello
4. Server receives Hello and responds
5. Client receives server's Hello

**Expected behavior**:

- Server logs incoming Hello packet
- Server sends Hello response
- Client receives and parses Hello
- Bidirectional communication verified

**Assertions**:

```rust
// Server side
assert!(server_output.contains("Hello") || server_output.contains("neighbor"));

// Client side
assert!(client_output.contains("Hello") || client_output.contains("received"));
```

**LLM calls**: 4 (server startup, client startup, server receives, client receives)

---

## Known Issues

### 1. Network Interface Requirement

**Issue**: Tests use hardcoded IP addresses (192.168.1.100, 192.168.1.101)

**Impact**: Tests may fail if these IPs are not available on the system

**Workaround**: Use loopback (127.0.0.1) or dynamically detect available IPs

**Future fix**: Add interface detection and dynamic IP allocation

### 2. Multicast Permission

**Issue**: Some systems require additional multicast permissions beyond root

**Impact**: Tests pass initialization but fail on multicast join

**Workaround**: Check firewall rules: `iptables -A INPUT -d 224.0.0.0/4 -j ACCEPT`

**Future fix**: Add multicast capability check in test setup

### 3. Timing Sensitivity

**Issue**: OSPF protocol has timing constraints (Hello interval, Dead interval)

**Impact**: Tests may be flaky if delays are too short

**Current delays**:

- Initialization: 1 second
- Hello exchange: 3 seconds

**Future fix**: Make delays configurable or use synchronization primitives

### 4. Test Isolation

**Issue**: Multiple OSPF tests running concurrently may interfere via multicast

**Impact**: Tests may receive packets from other test instances

**Workaround**: Use `--test-threads=1` for sequential execution

**Future fix**: Use different router IDs or area IDs for test isolation

## Running Tests

### Quick Test (Single Protocol)

```bash
# With root
sudo -E ./cargo-isolated.sh test --no-default-features --features ospf

# Just OSPF client tests
sudo -E ./cargo-isolated.sh test --no-default-features --features ospf --test client::ospf::e2e_test
```

### Full Test Suite

```bash
# All client tests (requires root for OSPF)
sudo -E ./cargo-isolated.sh test --features tcp,http,redis,ospf

# All protocols (slow, 1-2 min)
sudo -E ./cargo-isolated.sh test --all-features
```

### CI/CD Considerations

**GitHub Actions**: Use runners with root access or skip OSPF tests

```yaml
- name: Run OSPF tests
  if: runner.os == 'Linux'
  run: |
    sudo -E cargo test --no-default-features --features ospf
```

**Docker**: Run tests in privileged containers

```dockerfile
docker run --privileged --rm netget-test cargo test --features ospf
```

## Performance

### Expected Runtime

| Test                            | With Root       | Without Root   |
|---------------------------------|-----------------|----------------|
| test_ospf_client_initialization | ~2 seconds      | Instant (skip) |
| test_ospf_client_send_hello     | ~3 seconds      | Instant (skip) |
| test_ospf_client_with_server    | ~5 seconds      | Instant (skip) |
| **Total**                       | **~10 seconds** | **Instant**    |

### LLM Impact

**Ollama model**: qwen3-coder:30b (default)

**Per-call latency**:

- Fast hardware: ~1-2 seconds
- Slow hardware: ~5-10 seconds

**Total LLM time**: 7 calls × 1-2s = 7-14 seconds

**Network overhead**: Minimal (localhost only)

## Future Enhancements

### Priority 1: Interface Auto-Detection

Automatically detect available network interfaces:

```rust
fn get_available_interface() -> Result<Ipv4Addr> {
    // Use if_addrs crate to enumerate interfaces
    // Pick first non-loopback interface with IPv4
}
```

### Priority 2: LSA Parsing Tests

Test LSA (Link State Advertisement) parsing:

```rust
#[tokio::test]
async fn test_ospf_client_parse_lsa() {
    // Start server that sends LSU packet
    // Client receives and parses LSA content
    // Verify Router LSA links are parsed correctly
}
```

### Priority 3: Topology Discovery Test

Test full topology discovery:

```rust
#[tokio::test]
async fn test_ospf_client_topology_discovery() {
    // Start multiple OSPF servers (3+ routers)
    // Client queries each router for LSDB
    // Client builds topology graph
    // Verify all routers and links discovered
}
```

### Priority 4: Privilege Escalation Helper

Add helper to request privileges if missing:

```rust
fn ensure_root_privileges() -> Result<()> {
    if !has_root_privileges() {
        eprintln!("OSPF tests require root. Attempting privilege escalation...");
        // On Linux: Use sudo or polkit
        // On macOS: Use Authorization Services
    }
    Ok(())
}
```

## Test Maintenance

### Adding New Tests

1. Follow naming convention: `test_ospf_client_<feature>`
2. Document LLM call count in test doc comment
3. Update total LLM budget in this file
4. Add assertions for both success and failure cases

### Updating Tests

1. Keep LLM calls under 10 total
2. Maintain privilege checking
3. Update timing if protocol behavior changes
4. Test on both Linux and macOS

### Debugging Tests

**Enable verbose logging**:

```bash
RUST_LOG=debug sudo -E cargo test --features ospf
```

**Check raw socket operations**:

```bash
sudo tcpdump -i any -n proto 89
```

**Verify multicast membership**:

```bash
netstat -g | grep 224.0.0.5
```

## References

- [RFC 2328 - OSPFv2](https://datatracker.ietf.org/doc/html/rfc2328)
- [OSPF Server CLAUDE.md](../../../src/server/ospf/CLAUDE.md)
- [OSPF Client CLAUDE.md](../../../src/client/ospf/CLAUDE.md)
- [Test Infrastructure Guide](../../README.md)
