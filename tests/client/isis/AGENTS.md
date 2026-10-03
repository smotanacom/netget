# IS-IS Client E2E Tests

## Overview

End-to-end tests for the IS-IS (Intermediate System to Intermediate System) routing protocol client. These tests validate packet capture, PDU parsing, and LLM-driven topology analysis using Layer 2 packet capture.

## Critical Difference from Other Protocols

**IS-IS client captures Layer 2 traffic using pcap, NOT TCP/UDP sockets!**

Unlike client protocols like HTTP or Redis that connect to `host:port`, the IS-IS client:

- **Captures on interface**: Uses interface name (`eth0`, `veth1`) not IP address
- **Requires root**: pcap needs CAP_NET_RAW capability
- **Passive only**: Captures traffic, doesn't send (unlike TCP client)
- **No connection state**: Each PDU is independent (no handshake)

## Test Strategy

### Mock-Based Testing (Default)

Tests use Ollama mocks by default. This validates:
- LLM client startup (open_client with interface parameter)
- Event handling (isis_pdu_received event)
- Action generation (wait_for_more action)
- Memory updates (topology analysis)
- Mock verification

**What mocks DON'T test**: Actual packet capture (requires root + real traffic)

### Real Testing (--use-ollama)

Requires:
1. Root privileges (`sudo -E`)
2. IS-IS traffic on network (real router OR packet injection)
3. Network interface with traffic
4. Real Ollama instance

## Test Cases

### 1. `test_isis_client_startup` (1 LLM call)

**Purpose**: Verify client starts capturing on interface

**Mock Flow**:
```
User: "Connect to interface lo0 via IS-IS"
  ↓
LLM Mock: open_client action with remote_addr=interface name
  ↓
Client: Starts capture on lo0 (would require root to actually capture)
```

**LLM Calls**: 1 (startup)

**Assertions**:
- Client instance created
- Protocol is "IS-IS"
- Interface name is "lo0"
- Mock expectations verified

**Marked**: `#[ignore]` - requires root

### 2. `test_isis_client_capture_hello` (2 LLM calls)

**Purpose**: Verify client captures and analyzes Hello PDU

**Mock Flow**:
```
User: "Capture IS-IS on veth1, analyze Hello PDUs"
  ↓
LLM Mock 1: open_client action
  ↓
[Would inject Hello PDU on veth0, captured on veth1]
  ↓
LLM Mock 2: isis_pdu_received event → wait_for_more + memory update
```

**LLM Calls**:
- Startup: 1 call
- PDU received: 1 call

**Real Environment**:
```bash
# Terminal 1: Start client
sudo -E cargo run -- 'Capture IS-IS on veth1'

# Terminal 2: Inject PDU
sudo python3 <<EOF
from scapy.all import *
from scapy.contrib.isis import *

pdu = Ether(dst='01:80:c2:00:00:15')/
      LLC(dsap=0xfe, ssap=0xfe)/
      ISIS_CommonHdr()/
      ISIS_L2_LAN_IIH(sourceid='000000000002')

sendp(pdu, iface='veth0')
EOF
```

**Marked**: `#[ignore]` - requires root + packet injection

### 3. `test_isis_client_server_interaction` (3 LLM calls)

**Purpose**: Demonstrate full client-server IS-IS communication

**Mock Flow**:
- Server startup on veth0: 1 call
- Client startup on veth1: 1 call
- Client captures server's Hello: 1 call

**Setup Requirements**:
```bash
# Create veth pair
sudo ip link add veth0 type veth peer name veth1
sudo ip link set veth0 up
sudo ip link set veth1 up

# Run both server and client
sudo -E cargo run -- 'Start IS-IS server on veth0 with system-id 0000.0000.0001'
sudo -E cargo run -- 'Capture IS-IS on veth1'
```

**Expected Flow**:
1. Server sends Hello PDU on veth0
2. Client captures it on veth1
3. Client LLM analyzes PDU and identifies router

**Marked**: `#[ignore]` - requires root + veth pair

### 4. `test_isis_client_multiple_pdu_types` (5 LLM calls)

**Purpose**: Verify client handles different IS-IS PDU types

**Mock Flow**:
- Startup: 1 call
- L2 LAN Hello: 1 call
- L2 LSP: 1 call
- L2 CSNP: 1 call
- L2 PSNP: 1 call

**PDU Types Tested**:
- Type 16: L2 LAN Hello (neighbor discovery)
- Type 20: L2 LSP (topology information)
- Type 25: L2 CSNP (database sync)
- Type 27: L2 PSNP (LSP acknowledgment)

**Marked**: `#[ignore]` - requires root + IS-IS traffic

### 5. `test_isis_pdu_parsing` (0 LLM calls)

**Purpose**: Unit test for IS-IS PDU header validation

**Type**: Unit test (no network, no LLM, not ignored)

**Tests**:
- Discriminator byte (0x83)
- PDU type field (0x10 = L2 LAN Hello)
- Version field (0x01)

**Runs**: Always (no `#[ignore]`)

### 6. `test_device_listing_documentation` (0 LLM calls)

**Purpose**: Documentation for listing network interfaces

**Runs**: Always

### 7. `test_environment_requirements` (0 LLM calls)

**Purpose**: Documentation for test setup requirements

**Runs**: Always

## Total LLM Call Budget

**Mock Mode**: 11 calls across all tests
- test_isis_client_startup: 1
- test_isis_client_capture_hello: 2
- test_isis_client_server_interaction: 3
- test_isis_client_multiple_pdu_types: 5
- Unit tests: 0

Slightly over the < 10 guideline, but acceptable given client-server interaction test.

**Real Mode**: Same, but with actual Ollama and real traffic

## Running Tests

### Mock Mode (Default - No Ollama Required)

```bash
# Run all tests (only non-ignored)
cargo test --features isis --test client::isis::e2e_test

# Output:
# - test_isis_pdu_parsing: ✓ (runs)
# - test_device_listing_documentation: ✓ (runs)
# - test_environment_requirements: ✓ (runs)
# - test_isis_client_startup: ignored
# - test_isis_client_capture_hello: ignored
# - test_isis_client_server_interaction: ignored
# - test_isis_client_multiple_pdu_types: ignored
```

### Run Ignored Tests with Mocks (Root Required)

```bash
# Must run as root
sudo -E cargo test --features isis --test client::isis::e2e_test -- --ignored

# Will verify mocks but fail at pcap.open() without real traffic
```

### Real Mode with Ollama (Root + Traffic Required)

```bash
# Setup environment
sudo ip link add veth0 type veth peer name veth1
sudo ip link set veth0 up
sudo ip link set veth1 up

# Run tests
sudo -E cargo test --features isis --test client::isis::e2e_test -- --ignored --use-ollama

# Inject traffic from another terminal
```

## Expected Runtime

**Unit tests**: < 1 second
**Mock tests** (with root): 2-3 seconds per test (startup only)
**Real tests** (with Ollama + traffic): 5-60 seconds (waiting for PDUs)

## Known Issues

### pcap Limitations

1. **Platform-specific**: Interface names differ (eth0 vs en0 vs NPF_{GUID})
2. **Requires root**: CAP_NET_RAW capability needed
3. **Promiscuous mode**: May capture all traffic (privacy concern)
4. **Buffer overflow**: May miss PDUs in high-traffic environments

### Mock Limitations

Mocks verify LLM behavior but can't test:
- Actual packet capture (needs root + pcap)
- PDU parsing logic (happens before LLM)
- Interface existence checks
- pcap filter correctness

### Test Environment

**All meaningful tests marked `#[ignore]`** because they need root.

To run:
1. Must have root access
2. Must have network interface with IS-IS traffic OR
3. Must be able to inject IS-IS PDUs

Unlike TCP client tests that work on localhost, IS-IS needs real Layer 2 setup.

## Comparison with TCP Client Tests

| Aspect | TCP Client | ISIS Client |
|--------|------------|-------------|
| **Connection** | `connect("host:port")` | Interface capture (`"eth0"`) |
| **Privileges** | User | Root (CAP_NET_RAW) |
| **Active/Passive** | Active (sends data) | Passive (captures only) |
| **Localhost** | ✓ Works | ✗ Need real interface |
| **Mock Testing** | Full E2E on 127.0.0.1 | LLM logic only |
| **Server Interaction** | Easy (both localhost) | Need veth pair |

## Testing Approaches

### Approach 1: Unit Tests Only (No Root)

Run only non-ignored tests:
```bash
cargo test --features isis --test client::isis::e2e_test
```

Tests LLM mock logic without network access.

### Approach 2: Mocks with Root (Partial)

Run with root but no traffic:
```bash
sudo -E cargo test --features isis --test client::isis::e2e_test -- --ignored
```

Tests startup logic, fails gracefully when no traffic.

### Approach 3: Real Router (Full E2E)

Use FRRouting as IS-IS peer:
```bash
# Install FRR
sudo apt install frr
sudo sed -i 's/isisd=no/isisd=yes/' /etc/frr/daemons
sudo systemctl restart frr

# Configure IS-IS
sudo vtysh <<EOF
configure terminal
router isis MYNET
  net 49.0001.1921.6800.1001.00
  is-type level-2-only
exit
interface eth0
  ip router isis MYNET
  isis circuit-type level-2-only
exit
write
EOF

# Run tests
sudo -E cargo test --features isis --test client::isis::e2e_test -- --ignored --use-ollama
```

### Approach 4: Packet Injection (Controlled)

Use scapy for precise PDU injection:
```python
#!/usr/bin/env python3
from scapy.all import *
from scapy.contrib.isis import *

def inject_hello(iface, system_id):
    pdu = Ether(dst='01:80:c2:00:00:15')/
          LLC(dsap=0xfe, ssap=0xfe)/
          ISIS_CommonHdr()/
          ISIS_L2_LAN_IIH(
              sourceid=system_id,
              holdingtime=30
          )
    sendp(pdu, iface=iface, verbose=False)
    print(f"Injected Hello from {system_id} on {iface}")

# Run test sequence
inject_hello('veth0', '000000000002')
time.sleep(1)
inject_hello('veth0', '000000000003')
```

## Future Enhancements

1. **pcap File Replay**: Test with pre-captured traffic files
2. **Mock Interface**: Fake pcap device for testing without root
3. **Docker Environment**: Automated veth + FRR setup
4. **Integration Tests**: Combined client-server scenarios
5. **Topology Validation**: Verify LLM correctly builds network graph

## Debugging

### Check Interface Availability

```bash
# List all interfaces
ip link show

# Check if interface exists
ip link show eth0
```

### Test pcap Access

```bash
# Check permissions
ls -l /dev/bpf*  # macOS
getcap `which tcpdump`  # Linux

# Test pcap
sudo tcpdump -i eth0 -c 1
```

### Inject Test Traffic

```bash
# Simple ping to verify interface works
ping -I eth0 8.8.8.8

# Inject test IS-IS PDU
sudo python3 inject_isis_hello.py eth0
```

## References

- IS-IS Protocol: ISO/IEC 10589, RFC 1195
- Test implementation: `tests/client/isis/e2e_test.rs`
- Client implementation: `src/client/isis/CLAUDE.md`
- pcap: https://www.tcpdump.org/
- scapy IS-IS: https://scapy.readthedocs.io/en/latest/layers/isis.html
