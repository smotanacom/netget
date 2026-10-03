# IS-IS Server E2E Tests

## Overview

End-to-end tests for the IS-IS (Intermediate System to Intermediate System) routing protocol server. These tests validate neighbor discovery, Hello PDU exchange, and basic protocol operations using Layer 2 packet capture.

## Critical Difference from Other Protocols

**IS-IS uses Layer 2 (pcap), NOT TCP/UDP!**

Unlike protocols like TCP, HTTP, or DNS that run over IP sockets, IS-IS operates directly on the data link layer using raw Ethernet frames. This means:

- **No localhost testing**: Can't use `127.0.0.1:port` like TCP
- **Requires root**: pcap needs CAP_NET_RAW capability
- **Interface-based**: Uses interface names (`eth0`, `veth0`) not ports
- **Raw frames**: Ethernet + LLC/SNAP + IS-IS PDU structure

## Startup-failure coverage lives elsewhere

`tests/capture_startup_reports_failure_test.rs` (`isis_spawn_reports_unknown_device`,
`isis_spawn_outcome_matches_capture_privilege`) is the only IS-IS test in the tree that runs
**unprivileged and asserts something**. It pins the invariant that `spawn_with_llm_actions` must
not return `Ok` unless the pcap handle genuinely opened.

That mattered: IS-IS shipped the fire-and-forget `spawn_blocking` shape - the blocking task
logged and returned on failure while `spawn` had already committed to `Ok(interface)` - so an
unprivileged user got a server sitting in `Running` having captured nothing. ARP, DataLink and
ICMP had the same bug and were fixed one at a time; IS-IS was missed all three times. The guard
covers all four in one file for that reason, and asserts both branches so it fails on an
unprivileged runner (which is every one of them).

Everything below still requires root and is `#[ignore]`d.

## Test Strategy

### Mock-Based Testing (Default)

Tests use Ollama mocks by default. This validates:
- LLM action parsing (open_server with interface parameter)
- Event handling (isis_hello event)
- Action generation (send_isis_hello action)
- Mock verification

**What mocks DON'T test**: Actual packet capture/sending (requires root + real interfaces)

### Real Testing (--use-ollama)

Requires:
1. Root privileges (`sudo -E`)
2. Virtual network interfaces (veth pairs)
3. Packet injection tool (scapy, raw sockets)
4. Real Ollama instance

## Test Cases

### 1. `test_isis_server_startup` (1 LLM call)

**Purpose**: Verify server starts with correct interface configuration

**Mock Flow**:
```
User: "Start IS-IS router on lo0 with system-id 0000.0000.0001"
  ↓
LLM Mock: open_server action with interface (flexible binding) + startup_params (system_id, etc.)
  ↓
Server: IS-IS server on interface lo0 (would require root to actually start)
```

**LLM Calls**:
- Startup: 1 call

**Assertions**:
- Server instance created
- Stack is "IS-IS"
- Mock expectations verified

**Marked**: `#[ignore]` - requires root

### 2. `test_isis_hello_pdu_exchange` (2 LLM calls)

**Purpose**: Verify Hello PDU handling

**Mock Flow**:
```
User: "Start IS-IS router on veth0, respond to Hello PDUs"
  ↓
LLM Mock 1: open_server action
  ↓
[Would inject Hello PDU via raw socket on veth1]
  ↓
LLM Mock 2: isis_hello event → send_isis_hello action
```

**LLM Calls**:
- Startup: 1 call
- Hello received: 1 call

**Real Environment Requirements**:
```bash
# Create veth pair
sudo ip link add veth0 type veth peer name veth1
sudo ip link set veth0 up
sudo ip link set veth1 up

# Inject ISIS Hello PDU on veth1 using scapy:
from scapy.all import *
from scapy.contrib.isis import *

pdu = Ether(dst='01:80:c2:00:00:15')/
      LLC(dsap=0xfe, ssap=0xfe)/
      ISIS_CommonHdr()/
      ISIS_L2_LAN_IIH(
          sourceid='000000000002',
          holdingtime=30
      )

sendp(pdu, iface='veth1')
```

**Marked**: `#[ignore]` - requires root + veth setup

### 3. `test_isis_multiple_neighbors` (4 LLM calls)

**Purpose**: Verify multiple neighbor discovery

**Mock Flow**:
- Startup: 1 call
- 3 Hello PDUs from different neighbors: 3 calls

**Would inject**:
- Hello from 0000.0000.0002
- Hello from 0000.0000.0003
- Hello from 0000.0000.0004

**Marked**: `#[ignore]` - requires root + veth setup

### 4. `test_isis_pdu_structure` (0 LLM calls)

**Purpose**: Validate IS-IS PDU structure constants

**Type**: Unit test (no network, no LLM, not ignored)

**Tests**:
- Ethernet header structure (14 bytes)
- LLC/SNAP header (8 bytes, DSAP/SSAP 0xFE)
- IS-IS header (discriminator 0x83, PDU type 16)

**Runs**: Always (no `#[ignore]`)

### 5. `test_environment_requirements` (0 LLM calls)

**Purpose**: Documentation test explaining setup requirements

**Type**: Unit test (prints setup instructions)

**Runs**: Always (no `#[ignore]`)

## Total LLM Call Budget

**Mock Mode (default)**: 7 calls across all tests
- Startup tests: 1 + 2 + 4 = 7
- Unit tests: 0

**Real Mode (--use-ollama)**: Same, but with actual Ollama

All tests stay well under the < 10 calls per suite guideline.

## Running Tests

### Mock Mode (Default - No Ollama Required)

```bash
# Run all tests (only non-ignored tests will run)
cargo test --features isis --test server::isis::e2e_test

# Output:
# - test_isis_pdu_structure: ✓ (runs)
# - test_environment_requirements: ✓ (runs)
# - test_isis_server_startup: ignored
# - test_isis_hello_pdu_exchange: ignored
# - test_isis_multiple_neighbors: ignored
```

### Run Ignored Tests with Mocks (Root Required)

```bash
# Must run as root for pcap
sudo -E cargo test --features isis --test server::isis::e2e_test -- --ignored

# Tests will fail at pcap.open() but mocks will be verified
```

### Real Mode with Ollama (Root + Veth Required)

```bash
# Setup veth pair first
sudo ip link add veth0 type veth peer name veth1
sudo ip link set veth0 up
sudo ip link set veth1 up

# Run tests
sudo -E cargo test --features isis --test server::isis::e2e_test -- --ignored --use-ollama

# In another terminal, inject IS-IS PDUs on veth1
```

## Expected Runtime

**Unit tests**: < 1 second (test_isis_pdu_structure, test_environment_requirements)
**Mock tests** (if root available): 2-3 seconds per test (startup only, no traffic)
**Real tests** (with Ollama + packet injection): 5-30 seconds per test

## Known Issues

### Layer 2 Limitations

1. **Can't test on localhost**: IS-IS doesn't use IP sockets
2. **Requires root**: pcap needs elevated privileges
3. **Platform-specific**: veth (Linux), utun (macOS), different on Windows
4. **Packet injection complex**: Need scapy or raw socket programming

### Mock Limitations

Mocks verify LLM behavior but can't test:
- Actual packet capture (requires root + pcap)
- PDU parsing (not exposed to mocks)
- Raw frame construction
- LLC/SNAP encapsulation

### Test Isolation

**All tests marked `#[ignore]`** because they require root.

To run any meaningful tests:
1. Must run as root: `sudo -E`
2. Must setup veth interfaces
3. Must inject packets (manually or via script)

This is fundamentally different from TCP/HTTP tests that work on localhost without privileges.

## Comparison with TCP Tests

| Aspect | TCP Tests | ISIS Tests |
|--------|-----------|------------|
| **Transport** | TCP sockets (Layer 4) | Raw Ethernet (Layer 2) |
| **Connection** | `127.0.0.1:port` | Interface name (`veth0`) |
| **Privileges** | User | Root (CAP_NET_RAW) |
| **Localhost** | ✓ Works | ✗ Doesn't work |
| **Mock Testing** | Full E2E | LLM logic only |
| **Client-Server** | Easy (both on localhost) | Need veth pair |

## Testing Without Root

**Option 1**: Use `setcap` to grant specific binary CAP_NET_RAW:
```bash
sudo setcap cap_net_raw+ep target/debug/netget
cargo test --features isis --test server::isis::e2e_test -- --ignored
```

**Option 2**: Run in Docker container with `--cap-add=NET_RAW`

**Option 3**: Use packet replay files instead of live capture (future enhancement)

## Future Enhancements

1. **Packet Replay Testing**: Use pre-captured pcap files
2. **Mock Interface**: Create fake pcap device for testing
3. **Docker Test Environment**: Automated veth setup + FRR router
4. **Scapy Test Helper**: Python script for packet injection
5. **Non-Root Mode**: Fallback to pcap file replay when not root

## References

- IS-IS Protocol: ISO/IEC 10589, RFC 1195
- Test implementation: `tests/server/isis/e2e_test.rs`
- Server implementation: `src/server/isis/CLAUDE.md`
- pcap documentation: https://www.tcpdump.org/
