# ARP Client E2E Test Strategy

## Overview

End-to-end tests for ARP client implementation using black-box testing methodology. Tests verify that the ARP client can
capture and send ARP packets on network interfaces with LLM control.

## Test Approach

**Strategy**: Black-box E2E testing via NetGet CLI
**LLM Budget**: < 10 LLM calls total across all tests
**Runtime**: ~5-10 seconds per test (excluding LLM latency)

## Prerequisites

### Root Privileges Required

ARP packet capture and injection require elevated privileges:

- **Linux**: Run tests with `sudo` or grant CAP_NET_RAW capability
- **macOS**: Run tests with `sudo`
- **Windows**: Run as Administrator

**Capability Grant** (Linux, preferred over sudo):

```bash
sudo setcap cap_net_raw+ep target-claude/release/netget
```

### Network Interface

Tests use loopback interface for safety:

- **Linux**: `lo`
- **macOS**: `lo0`
- **Windows**: Platform-specific

**Why Loopback**:

- Always available
- No external network traffic
- Safe for testing (no risk of disrupting real network)

### System Dependencies

- **libpcap**: Required for packet capture (usually pre-installed)
- **Permissions**: Root or CAP_NET_RAW capability

## Test Cases

### 1. test_arp_client_start_on_interface

**Purpose**: Verify ARP client can start and monitor a network interface
**LLM Calls**: 1 (client startup)
**Duration**: ~2-3 seconds

**Test Flow**:

1. Check if running as root (skip if not)
2. Get loopback interface name (`lo` or `lo0`)
3. Start ARP client with instruction: "Monitor ARP traffic on interface {interface}"
4. Wait 1 second for client to initialize
5. Verify client output contains "ARP" keyword
6. Stop client and cleanup

**Success Criteria**:

- Client starts without errors
- Client shows ARP protocol in output
- Client binds to specified interface

**Failure Modes**:

- Not running as root → Skip test
- Interface not found → Error
- pcap permissions denied → Error

### 2. test_arp_client_send_request

**Purpose**: Verify ARP client can send ARP request (who-has query)
**LLM Calls**: 2 (client startup, send request action)
**Duration**: ~3-4 seconds

**Test Flow**:

1. Check if running as root
2. Get loopback interface
3. Start ARP client with instruction: "Monitor ARP on interface {interface}. Send who-has query for 127.0.0.1."
4. Wait 1.5 seconds for client to send request
5. Verify client protocol is "ARP"
6. Stop client and cleanup

**Success Criteria**:

- Client sends ARP request without errors
- Client protocol correctly identified as "ARP"
- No crashes or panics

**Failure Modes**:

- Not root → Skip
- Packet injection fails → Error
- Invalid MAC/IP address → LLM error

### 3. test_arp_client_monitor_traffic

**Purpose**: Verify ARP client can passively monitor ARP traffic
**LLM Calls**: 1 (client startup)
**Duration**: ~2-3 seconds

**Test Flow**:

1. Check if running as root
2. Get loopback interface
3. Start ARP client with instruction: "Monitor all ARP traffic on interface {interface}. Log all ARP packets."
4. Wait 1 second for monitoring to start
5. Verify client output contains "ARP" or "started"
6. Stop client and cleanup

**Success Criteria**:

- Client enters monitoring mode
- Client shows it's listening for ARP packets
- No errors or crashes

**Failure Modes**:

- Not root → Skip
- Capture setup fails → Error
- Interface doesn't exist → Error

## LLM Call Budget

| Test                               | LLM Calls | Rationale                 |
|------------------------------------|-----------|---------------------------|
| test_arp_client_start_on_interface | 1         | Startup only              |
| test_arp_client_send_request       | 2         | Startup + send action     |
| test_arp_client_monitor_traffic    | 1         | Startup only              |
| **Total**                          | **4**     | Well under 10 call budget |

## Expected Runtime

- **Per Test**: 2-5 seconds (setup/teardown)
- **LLM Latency**: 2-5 seconds per call
- **Total Suite**: ~15-30 seconds (with LLM)
- **Parallel**: Not recommended (root required, shared interfaces)

## Known Issues

### 1. Root Privilege Requirement

**Issue**: Tests skip if not running as root
**Workaround**: Run with `sudo ./cargo-isolated.sh test --features arp --test client::arp::e2e_test`
**Impact**: CI/CD must run tests with elevated privileges

### 2. Platform-Specific Interface Names

**Issue**: Loopback interface name varies by OS (lo vs lo0)
**Mitigation**: Platform detection in `get_loopback_interface()`
**Impact**: Tests should work across Linux/macOS/Windows

### 3. pcap Library Availability

**Issue**: libpcap must be installed on system
**Workaround**: Pre-install libpcap (usually available by default)
**Impact**: Docker/CI environments need libpcap-dev

### 4. No Actual ARP Traffic on Loopback

**Issue**: Loopback interface typically doesn't generate ARP traffic
**Workaround**: Tests focus on client startup, not actual packet capture
**Impact**: Tests verify client functionality, not real ARP capture

### 5. Flaky Tests Due to Timing

**Issue**: pcap initialization may take variable time
**Mitigation**: Use generous sleep durations (1-1.5 seconds)
**Impact**: Tests may occasionally timeout, increase sleep if needed

## Test Infrastructure

### Helper Functions

#### is_root()

Checks if current process has root privileges:

```rust
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
```

#### get_loopback_interface()

Returns platform-specific loopback interface name:

```rust
fn get_loopback_interface() -> E2EResult<String> {
    #[cfg(target_os = "linux")]
    Ok("lo".to_string())

    #[cfg(target_os = "macos")]
    Ok("lo0".to_string())

    // ...
}
```

### Test Utilities

Uses standard E2E helpers from `tests/helpers/`:

- `start_netget_client()`: Spawn client process
- `NetGetConfig`: Configure client instruction
- `client.output_contains()`: Check output for keywords
- `client.stop()`: Cleanup client process

## Running Tests

### Basic Test Run (as root)

```bash
sudo ./cargo-isolated.sh test --no-default-features --features arp --test client::arp::e2e_test
```

### With Capability Grant (Linux, no sudo)

```bash
# Grant capability once
sudo setcap cap_net_raw+ep target-claude/release/netget

# Run tests without sudo
./cargo-isolated.sh test --no-default-features --features arp --test client::arp::e2e_test
```

### Individual Test

```bash
sudo ./cargo-isolated.sh test --no-default-features --features arp --test client::arp::e2e_test -- test_arp_client_start_on_interface
```

### Verbose Output

```bash
sudo ./cargo-isolated.sh test --no-default-features --features arp --test client::arp::e2e_test -- --nocapture
```

## CI/CD Considerations

### GitHub Actions

```yaml
- name: Run ARP Client Tests
  run: |
    sudo setcap cap_net_raw+ep target-claude/release/netget
    ./cargo-isolated.sh test --no-default-features --features arp --test client::arp::e2e_test
```

### Docker

```dockerfile
# Install libpcap
RUN apt-get update && apt-get install -y libpcap-dev

# Grant capability
RUN setcap cap_net_raw+ep /app/netget

# Run tests
CMD ["cargo", "test", "--features", "arp"]
```

## Security Considerations

### Test Safety

- **Local Only**: Tests use loopback interface (no external network)
- **No Spoofing**: Tests don't send malicious ARP packets
- **Privilege Isolation**: Tests skip if not root (safe failure)
- **Cleanup**: All clients properly stopped and cleaned up

### Risk Mitigation

- Loopback interface ensures no network disruption
- Test timeouts prevent hung processes
- Proper process cleanup in all code paths
- No persistent state or configuration changes

## Future Improvements

1. **Mock pcap**: Use virtual network interfaces for testing without root
2. **Traffic Generation**: Send actual ARP packets for integration testing
3. **Multi-Interface**: Test on multiple interfaces simultaneously
4. **Performance**: Measure packet capture latency
5. **Stress Testing**: High-volume ARP traffic handling

## References

- [libpcap Documentation](https://www.tcpdump.org/manpages/pcap.3pcap.html)
- [ARP RFC 826](https://datatracker.ietf.org/doc/html/rfc826)
- [NetGet E2E Test Infrastructure](../../README.md)
