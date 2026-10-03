# ARP Protocol E2E Tests

## What actually runs — read this before the rest

Most of this file describes `e2e_test.rs`, and **`e2e_test.rs` has never run and cannot
run as written.** It is `#[ignore]`d, and unlike most ignored tests `sudo` does not
release it: it targets the **loopback** interface, and ARP's `spawn()` refuses loopback
outright. `arp` is an Ethernet-only BPF keyword, so on DLT_NULL/DLT_LOOP libpcap compiles
the filter to "expression rejects all packets" and errors —
`tests/capture_startup_reports_failure_test.rs::arp_spawn_refuses_loopback_and_says_why`
asserts that refusal in both the privileged and unprivileged branches. Exercising the
scenario needs a real Ethernet or Wi-Fi interface *and* root, and would inject ARP frames
onto that live segment.

**`frame_codec_test.rs` is the ARP server evidence that actually runs**, in every ordinary
test run, needing no interface and no privilege: five tests asserting the emitted reply
field by field against the Ethernet II + RFC 826 layout, that the declared `send_arp_reply`
example executes to those same bytes, that `ignore_arp` produces no output at all, and that
nine malformed actions are refused rather than panicking.

Everything below about LLM budgets, runtimes, pass rates and graceful skipping describes
the *intent* of `e2e_test.rs`. None of it has ever been measured. The numbers were written
from expectation, not observation — treat them as a specification for a future rewrite
against a real interface, not as a record.

## Test Overview

End-to-end tests for ARP (Address Resolution Protocol) server functionality. Tests spawn NetGet ARP server and validate
Layer 2 packet handling using pcap for packet capture/injection and pnet for packet construction.

**Protocols Tested**: ARP REQUEST and ARP REPLY (RFC 826)

## Test Strategy

**pcap + pnet Approach**: Tests use real packet capture and injection:

- `pcap` for raw packet capture and injection (requires CAP_NET_RAW or root)
- `pnet` for structured ARP packet construction and parsing
- Loopback interface testing (no external network required)
- Real wire-format validation

**Structured Packet Construction**: pnet builds Ethernet + ARP frames:

- Ensures RFC 826 compliance
- Tests exact wire format (Ethernet header + ARP packet)
- Type-safe packet building (no manual hex encoding)

**Black-Box Protocol Testing**: Tests validate only external behavior (ARP request → ARP reply), not internal LLM
prompts or implementation details.

**Comprehensive Single-Server Approach**: One server handles all test cases with scripting for maximum efficiency.

## LLM Call Budget

### Current Implementation

**Single Comprehensive Test** (`test_arp_responder`):

- 1 server startup (with comprehensive scripting instructions) = **1-2 LLM calls**
- 3 ARP requests (all handled by script) = **0 LLM calls**
- **Total: 1-2 LLM calls** ✅ **Target met: < 10 calls**

### Test Breakdown

The single test validates:

1. ARP request for 192.168.1.100 → Reply with aa:bb:cc:dd:ee:ff
2. ARP request for 192.168.1.101 → Reply with 11:22:33:44:55:66
3. ARP request for 192.168.1.200 → No reply (ignored)

**All handled by scripting after initial server setup.**

## Scripting Usage

**Scripting HEAVILY Used** ✅: ARP is PERFECT for scripting because:

- **Deterministic**: Target IP → MAC address mapping
- **Stateless**: No session tracking
- **Simple logic**: If-then rules for IP-to-MAC mappings
- **Well-defined**: RFC 826 specifies exact packet format

**Script Logic** (conceptual):

```python
def handle_arp_request(event):
    target_ip = event['target_ip']
    sender_ip = event['sender_ip']
    sender_mac = event['sender_mac']

    # IP-to-MAC mappings
    mappings = {
        '192.168.1.100': 'aa:bb:cc:dd:ee:ff',
        '192.168.1.101': '11:22:33:44:55:66'
    }

    if target_ip in mappings:
        return {
            'type': 'send_arp_reply',
            'sender_mac': mappings[target_ip],
            'sender_ip': target_ip,
            'target_mac': sender_mac,
            'target_ip': sender_ip
        }
    else:
        return {'type': 'ignore_arp'}
```

**Why Scripting Works**: ARP is simple key-value lookup (IP → MAC). No computation needed.

## Client Library

**pnet + pcap** - Raw packet construction and injection

- **`pcap::Capture`**: Raw packet capture and injection (requires privileges)
- **`pnet::packet::arp`**: Structured ARP packet building
- **`pnet::packet::ethernet`**: Ethernet frame construction
- **Manual wire-level testing**: Complete control over packet format

**Why This Approach**: ARP is a Layer 2 protocol, standard network libraries (like std::net) don't support it. Must use
raw sockets.

**Helper Functions**:

```rust
fn build_arp_request(sender_mac: MacAddr, sender_ip: Ipv4Addr, target_ip: Ipv4Addr) -> Vec<u8>;
fn find_loopback_interface() -> Result<String, Box<dyn std::error::Error>>;
```

## Expected Runtime

**Model**: qwen3-coder:30b (default NetGet model)

**Runtime**: ~10-20 seconds for full test suite

- Server startup: ~5-10 seconds (LLM generates script)
- 3 ARP requests: ~3-6 seconds (pcap capture timeouts)
- Packet injection/capture: <1ms per request
- Timeout waits: 3-10 seconds (waiting for replies or no-reply confirmation)

**Note**: `--ollama-lock` does nothing — the flag is inert and its plumbing was deleted.
LLM concurrency is bounded by `--llm-max-concurrent` / `--llm-max-queued`.

**Note**: ARP tests may be slower due to pcap timeout windows (need to wait to confirm no reply).

## Failure Rate

**Historical Flakiness**: unmeasured. The test has never completed a run, so no pass
rate exists. The percentages below were written from expectation and are retained only as
a list of the failure modes a rewrite would have to handle.

**Why Less Stable Than Other Tests**:

- **Requires elevated privileges**: Root/CAP_NET_RAW needed, may fail in CI
- **Platform-dependent**: Interface names vary (lo, lo0, loopback)
- **Timing-sensitive**: pcap capture windows may miss packets
- **Loopback quirks**: Some systems don't route ARP on loopback

**Common Failure Modes**:

1. **Insufficient Privileges** (~10% of runs, environment-dependent)
    - Symptom: Test skipped with "requires CAP_NET_RAW or root" message
    - Cause: Not running with raw socket capabilities
    - Mitigation: Run tests with sudo or grant CAP_NET_RAW capability

2. **Loopback Interface Not Found** (~2% of runs)
    - Symptom: "No loopback interface found" error
    - Cause: Platform doesn't have standard "lo" or "lo0" interface
    - Mitigation: Test detects and skips gracefully

3. **Packet Capture Timeout** (~5-10% of runs)
    - Symptom: "Timeout waiting for ARP reply" warning (may be expected)
    - Cause: ARP reply not captured within timeout window, or loopback doesn't support ARP
    - Mitigation: Test treats timeout as warning (some loopback interfaces don't support ARP)

4. **LLM Fails to Generate Script** (~3% of runs)
    - Symptom: Server doesn't respond or responds incorrectly
    - Cause: LLM doesn't understand IP-to-MAC mapping instructions
    - Mitigation: Retry test; if persistent, simplify prompt

**Most Stable Aspects**:

- Packet construction: pnet ensures valid packets
- Interface detection: Cross-platform loopback detection
- Graceful degradation: Tests warn but don't fail on timeout (may be expected)

**Least Stable Aspects**:

- Loopback ARP behavior: Varies by OS/kernel version
- Timing windows: pcap timeout must balance speed vs reliability

## Test Cases Covered

### ARP Response Mapping

1. **ARP Request for 192.168.1.100** (successful mapping)
    - Validates reply with MAC aa:bb:cc:dd:ee:ff
    - Tests IP-to-MAC mapping logic
    - Verifies Ethernet framing and ARP packet structure

2. **ARP Request for 192.168.1.101** (successful mapping)
    - Validates reply with MAC 11:22:33:44:55:66
    - Tests second mapping entry
    - Confirms consistent behavior

3. **ARP Request for 192.168.1.200** (unmapped IP)
    - Validates no reply sent
    - Tests ignore logic for unknown IPs
    - Confirms selective response behavior

### Coverage Gaps

**Not Yet Tested**:

- **ARP REPLY packets**: Only ARP REQUEST tested (server doesn't need to handle replies)
- **Gratuitous ARP**: Sender IP = target IP (ARP announcement)
- **Proxy ARP**: Responding for IP on different segment
- **ARP probes**: Sender IP = 0.0.0.0 (address conflict detection)
- **RARP (Reverse ARP)**: MAC → IP lookup (RFC 903, obsolete)
- **IPv6 NDP**: Neighbor Discovery Protocol (IPv6 equivalent of ARP)
- **Non-Ethernet hardware**: Only tested with Ethernet (ARP supports other L2)
- **VLAN tagged frames**: 802.1Q VLAN tagging
- **Malformed packets**: Invalid hardware/protocol types, wrong lengths
- **ARP cache manipulation**: Verify host OS doesn't accept fake replies
- **High-frequency requests**: ARP flood handling
- **Multiple interfaces**: Capture on multiple interfaces simultaneously

## Test Infrastructure

### Privilege Requirements

**CAP_NET_RAW or Root Required**: ARP requires raw socket access for:

- Promiscuous mode packet capture
- Raw packet injection at Layer 2
- Bypassing OS network stack

**The in-test guard does NOT detect the privilege it claims to.** On macOS any
unprivileged user can call `Device::list()` successfully — it is *opening* a handle that
fails — so this guard never fires and the test would proceed to fail for a different
reason. It is the same mistake `src/privilege.rs` was fixed for (it now probes by opening
a handle). The `#[ignore]` is what actually keeps the test out of ordinary runs:

```rust
// Present in e2e_test.rs, and inert:
if Device::list().is_err() {
    println!("⚠ Skipping ARP test: requires CAP_NET_RAW or root privileges");
    return Ok(());
}
```

Note also that this shape — print a skip message and return `Ok(())` — is *not* evidence
even when it does fire. A skipped test is a silent pass. See the root `CLAUDE.md` on
skip-when-missing gates.

**CI Considerations**: CI runners may not grant raw socket access. Tests must tolerate skips.

### Loopback Interface Detection

**Cross-Platform Interface Discovery**:

- Linux: Usually "lo"
- macOS: Usually "lo0"
- Other: Searches for interface starting with "lo"

```rust
fn find_loopback_interface() -> Result<String, Box<dyn std::error::Error>> {
    let devices = Device::list()?;
    for device in devices {
        if device.name == "lo" || device.name == "lo0" || device.name.starts_with("lo") {
            return Ok(device.name);
        }
    }
    Err("No loopback interface found".into())
}
```

### Packet Construction

**`build_arp_request(sender_mac, sender_ip, target_ip)`**:

```rust
fn build_arp_request(
    sender_mac: MacAddr,
    sender_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
) -> Vec<u8> {
    // 42 bytes: 14 (Ethernet) + 28 (ARP)
    let mut eth_buffer = vec![0u8; 42];

    // Build Ethernet frame
    let mut eth_packet = MutableEthernetPacket::new(&mut eth_buffer).unwrap();
    eth_packet.set_destination(MacAddr::broadcast()); // ff:ff:ff:ff:ff:ff
    eth_packet.set_source(sender_mac);
    eth_packet.set_ethertype(EtherTypes::Arp); // 0x0806

    // Build ARP packet
    let mut arp_buffer = vec![0u8; 28];
    let mut arp_packet = MutableArpPacket::new(&mut arp_buffer).unwrap();
    arp_packet.set_hardware_type(ArpHardwareTypes::Ethernet); // 0x0001
    arp_packet.set_protocol_type(EtherTypes::Ipv4); // 0x0800
    arp_packet.set_hw_addr_len(6); // MAC address length
    arp_packet.set_proto_addr_len(4); // IPv4 address length
    arp_packet.set_operation(ArpOperations::Request); // 0x0001
    arp_packet.set_sender_hw_addr(sender_mac);
    arp_packet.set_sender_proto_addr(sender_ip);
    arp_packet.set_target_hw_addr(MacAddr::zero()); // 00:00:00:00:00:00
    arp_packet.set_target_proto_addr(target_ip);

    eth_packet.set_payload(&arp_buffer);
    eth_buffer
}
```

### Test Execution Pattern

```rust
// 1. Check privileges and find interface
if Device::list().is_err() {
    println!("⚠ Skipping: requires root");
    return Ok(());
}
let interface = find_loopback_interface()?;

// 2. Start ARP server with comprehensive scripting prompt
let config = ServerConfig::new(format!("listen on interface {} via arp\n...", interface));
let test_state = start_netget_server(config).await?;
tokio::time::sleep(Duration::from_secs(3)).await;

// 3. Open pcap for packet injection and capture
let mut cap = Capture::from_device(device)?
    .promisc(true)
    .timeout(5000)
    .open()?;
cap.filter("arp", true)?;

// 4. Build and inject ARP requests
for test_case in test_cases {
    let request = build_arp_request(sender_mac, sender_ip, target_ip);
    cap.sendpacket(&request)?;

    // 5. Capture and validate reply (with timeout)
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match cap.next_packet() {
                Ok(packet) => {
                    // Parse and validate ARP reply
                    if is_expected_reply(packet, target_ip, expected_mac) {
                        return Ok(());
                    }
                }
                Err(pcap::Error::TimeoutExpired) => continue,
                Err(e) => return Err(e),
            }
        }
    }).await;

    // Handle result (success, error, or timeout)
}

// 6. Cleanup
test_state.stop().await?;
```

## Known Issues

### Loopback ARP Behavior Varies by Platform

**Issue**: Some OSes don't process ARP on loopback interface
**Impact**: Tests may timeout waiting for replies (gracefully handled as warning)
**Platforms Affected**: Linux (sometimes), Windows (unpredictable)
**Mitigation**: Tests treat timeout as non-fatal warning

### pcap Capture Timing Windows

**Issue**: 5-10 second timeouts needed to confirm "no reply" cases
**Impact**: Tests slower than other protocols
**Benefit**: Reliable detection of ignored packets vs. missed packets

### Elevated Privilege Requirement

**Issue**: Tests require root or CAP_NET_RAW capability
**Impact**: May not run in all CI environments
**Mitigation**: Tests skip gracefully with clear message

### Platform-Specific Interface Names

**Issue**: Interface detection may fail on unusual systems
**Impact**: Test skipped on platforms without standard loopback
**Mitigation**: Cross-platform detection with fallback

## Running Tests

```bash
# Build release binary first (for performance)
./cargo-isolated.sh build --release --features arp

# Run all ARP tests (requires root/CAP_NET_RAW + Ollama + model)
sudo ./cargo-isolated.sh test --features arp --test server::arp::e2e_test

# Run with output
sudo ./cargo-isolated.sh test --features arp --test server::arp::e2e_test -- --nocapture

# Run with Ollama lock (prevent concurrent LLM overload)
sudo ./cargo-isolated.sh test --features arp --test server::arp::e2e_test -- --test-threads=1
```

**Important**: Must run with `sudo` or grant CAP_NET_RAW capability:

```bash
# Grant capability (persistent, no sudo needed)
sudo setcap cap_net_raw+ep target/release/netget
```

## Future Test Additions

1. **Gratuitous ARP**: Test ARP announcements (sender IP = target IP)
2. **ARP Probes**: Test address conflict detection (sender IP = 0.0.0.0)
3. **Proxy ARP**: Respond for IP on different segment
4. **Multiple Interfaces**: Capture on eth0 and eth1 simultaneously
5. **ARP Flood**: Test high-frequency ARP request handling
6. **Malformed Packets**: Invalid lengths, wrong hardware types
7. **VLAN Tagged**: Test 802.1Q VLAN support
8. **Performance**: Measure ARP responses/second with scripting
9. **Non-Ethernet Hardware**: Test Token Ring, FDDI (if supported by pnet)
10. **Real Network Testing**: Test on actual network interface (not loopback)

## Comparison to Target

**Target**: < 10 LLM calls
**Actual**: 1-2 LLM calls
**Achievement**: ✅ **85% reduction** from naive approach

**Naive Approach Would Be**:

- 3 test cases × 1 LLM call per request = 3 LLM calls minimum
- Plus server startup = 4 LLM calls total

**Scripting Optimization**: **2-3x improvement** (1-2 calls vs 4 calls)

## Success Criteria

Scored against what runs, not against what was hoped for.

| Criterion | `frame_codec_test.rs` | `e2e_test.rs` |
|---|---|---|
| Runs in an ordinary test run | ✅ five tests, no privilege, no interface | ❌ `#[ignore]`d, and cannot pass even under `sudo` |
| LLM calls | ✅ zero — pure functions | not measured; never ran |
| Runtime | ✅ <10ms | not measured; never ran |
| Coverage | wire format, the declared example, `ignore_arp`, nine rejection paths | request → reply over a real wire — the thing nothing has ever observed |
| Graceful degradation | n/a — nothing to skip | ❌ the `Device::list()` guard does not fire; see above |

**Maturity**: ARP stays **Experimental**, and the reason is narrower than this file used
to give. What is missing is not stability or platform coverage — it is that **no test has
ever observed a captured ARP request becoming a reply on a wire.** `frame_codec_test.rs`
proves the bytes are right; it cannot prove the capture, the filter, the injection thread
and the LLM round-trip compose. Promoting past Experimental needs:

1. A rewrite of `e2e_test.rs` against a real Ethernet or Wi-Fi interface (loopback is
   refused by design and always will be), accepting that it injects onto a live segment
   and so cannot be part of an ordinary run.
2. Somebody having actually run it, once, and said so here.
