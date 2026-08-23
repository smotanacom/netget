# ARP Protocol Implementation

## Overview

Layer 2 Address Resolution Protocol (ARP) server that captures and responds to ARP requests using libpcap and pnet.
Allows the LLM to respond to "who has" queries with custom MAC addresses, enabling ARP spoofing simulation, network
mapping experiments, and honeypot operations.

### Default behaviour: static (answer nothing), no LLM

An ARP reply is **not wire-determined** — the MAC advertised is a policy choice
(spoofing/honeypot/custom mapping), like DNS/DHCP. So with **no operator policy** (no server
instruction and no per-event handler), the server **answers nothing** — it has no MAC to claim —
and takes **no LLM round-trip per captured packet** (gated by `operator_wants_dynamic` in `mod.rs` =
`has_instruction || has_handler`). The LLM is consulted **only when the operator opts in** with the
mapping to serve (an instruction or a handler). The material below describing the model receiving
ARP events and returning `send_arp_reply` therefore describes the opt-in path, not the default.

### LLM failure: silence is the correct answer, and the log says which silence

ARP has **no error message**. The only frame this server can emit is a reply asserting that some
MAC owns the queried IP — and that MAC is exactly what the failed LLM call was supposed to decide.
Fabricating one would poison the requester's neighbour cache; a requester that hears nothing simply
times out, which is RFC 826's normal outcome for "nobody here owns that address". So on an LLM
error the server deliberately puts **nothing** on the wire. This is the documented exception to the
"answer the peer on backend failure" rule in the root `CLAUDE.md`, not an oversight.

Because all outcomes look identical on the wire, they are separated **in the log** by a `decision=`
tag, the same way `radius` separates its cases:

| Tag | Meaning |
|---|---|
| `decision=no_policy` | No instruction and no handler — static default, no LLM call at all |
| `decision=model_reject` | The model answered with `ignore_arp` — a real decision not to reply |
| `decision=model_no_answer` | The model returned no actions |
| `decision=fail_closed_overloaded` | The LLM call errored and `WireFailure::classify` said the backend is saturated (retryable) |
| `decision=fail_closed_llm_error` | The LLM call errored otherwise |

The full error goes to `tracing::error!` and the status stream (operator-facing, both local). It
never reaches a peer, because no packet is sent.

**Status**: Experimental (Layer 2 Protocol)
**Layer**: OSI Layer 2 (Data Link)
**Interface**: Any network interface (eth0, en0, wlan0, etc.)
**RFC**: [RFC 826 - Address Resolution Protocol](https://datatracker.ietf.org/doc/html/rfc826)

## Library Choices

### Core Libraries

- **pcap v2.2** - Rust bindings to libpcap
    - Low-level packet capture at data link layer
    - Promiscuous mode support (capture all ARP traffic on segment)
    - BPF filter automatically set to "arp" for efficiency
    - Blocking API (wrapped in tokio::task::spawn_blocking)
    - Packet injection via `sendpacket()` for ARP replies

- **pnet v0.35** - Packet parsing and construction
    - `pnet::packet::arp` - ARP packet parsing and building
    - `pnet::packet::ethernet` - Ethernet frame construction
    - Typed API for ARP operations (Request, Reply)
    - MAC address and IPv4 address handling

- **libpcap** (system library, required dependency)
    - Industry-standard packet capture library (used by tcpdump, Wireshark)
    - Cross-platform (Linux, macOS, Windows with WinPcap)
    - Requires elevated privileges (root/admin) for promiscuous mode and injection

**Rationale**:

- libpcap is the standard for low-level packet capture
- pnet provides structured ARP packet handling (vs raw hex parsing)
- Combination allows both capture and injection with minimal code

## Architecture Decisions

### 1. Blocking I/O with Tokio Bridge

pcap has blocking API, Tokio is async:

**Solution**:

- Spawn packet capture in `tokio::task::spawn_blocking()`
- Blocking task runs pcap capture loop with ARP filter
- For each ARP packet, spawn async task for processing
- **Default (no operator policy)**: answer nothing, **no LLM call**. **Opt-in only** (instruction or handler): the handler/LLM decides whether/how to respond
- Responses injected via `cap.sendpacket()` (blocking call from async task)

**Tradeoff**: Extra task spawning overhead, but necessary for pcap API compatibility.

### 2. Full ARP Packet Injection

Unlike DataLink (capture-only), ARP server can inject responses:

- LLM receives ARP request event
- LLM can return `send_arp_reply` action
- Action specifies sender/target MAC/IP addresses
- Server builds complete Ethernet frame with ARP reply
- Frame injected via pcap's `sendpacket()`

**Use Cases**:

1. ARP spoofing simulation (respond with fake MAC for any IP)
2. Custom IP-to-MAC mappings (map virtual IPs to real MACs)
3. Honeypot responses (reply to ARP scans with fake devices)
4. Network experimentation (test ARP behavior)

### 3. Structured Packet Parsing

ARP packets parsed with pnet (not raw hex):

- Ethernet frame parsed for source/destination MAC
- ARP packet parsed for operation, addresses
- LLM receives structured JSON with fields:
    - `operation`: "REQUEST" or "REPLY"
    - `sender_mac`: "aa:bb:cc:dd:ee:ff"
    - `sender_ip`: "192.168.1.100"
    - `target_mac`: "00:00:00:00:00:00"
    - `target_ip`: "192.168.1.1"

**Rationale**: ARP structure is well-defined (RFC 826). Structured parsing is more reliable than LLM hex parsing.

### 4. Interface Selection

User specifies network interface in prompt:

- Example: "Listen for ARP requests on eth0"
- Server uses `Device::list()` to find interface by name
- Fails if interface doesn't exist or lacks permissions

**Common Interfaces**:

- Linux: eth0, eth1, wlan0, lo
- macOS: en0, en1, lo0
- Windows: "\Device\NPF_{GUID}"

### 5. Automatic ARP Filtering

BPF filter automatically set to "arp":

- Kernel-level filtering (efficient, no user-space overhead)
- Only ARP packets (EtherType 0x0806) passed to LLM
- Reduces noise from other Layer 2 traffic

**Manual Override**: User can't override filter - it's always "arp" for this protocol.

### 6. Promiscuous Mode

Capture is always in **promiscuous mode**:

- Captures all ARP traffic on network segment (not just ARP for this host)
- Requires elevated privileges
- Essential for network monitoring and honeypot scenarios
- Allows detection of ARP requests for other hosts

**Security**: Only works on same network segment (same switch/hub). Can't capture ARP on other segments.

### 7. ARP Reply Builder

Helper function `build_arp_reply()` constructs complete response:

- Input: sender MAC/IP, target MAC/IP
- Output: 42-byte Ethernet frame (14 eth + 28 arp)
- Ethernet: destination=target_mac, source=sender_mac, type=ARP
- ARP: operation=Reply, hardware=Ethernet, protocol=IPv4

**LLM Integration**: LLM provides parameters, server builds binary packet.

### 8. Dual Logging

All operations use **dual logging**:

- **DEBUG**: ARP summary (operation, sender, target)
- **TRACE**: Full packet hex dump
- **INFO**: LLM messages and high-level analysis
- **ERROR**: Capture errors, pcap failures, permission issues
- All logs go to both `netget.log` (via tracing) and TUI Status panel (via status_tx)

## LLM Integration

### Action-Based Response Model

The LLM responds to ARP events with actions:

**Events**:

- `arp_request_received` - ARP packet captured from interface
    - Parameters: `operation`, `sender_mac`, `sender_ip`, `target_mac`, `target_ip`, `packet_hex`

**Available Actions**:

- `send_arp_reply` - Send ARP reply packet
    - Parameters: `sender_mac`, `sender_ip`, `target_mac`, `target_ip`
- `ignore_arp` - Don't respond to this ARP packet
- Common actions: `update_instruction`, etc.

### Example LLM Responses

**Respond to ARP Request**:

```json
{
  "actions": [
    {
      "type": "send_arp_reply",
      "sender_mac": "aa:bb:cc:dd:ee:ff",
      "sender_ip": "192.168.1.100",
      "target_mac": "11:22:33:44:55:66",
      "target_ip": "192.168.1.1"
    }
  ]
}
```

**Ignore ARP Request**:

```json
{
  "actions": [
    {
      "type": "ignore_arp"
    }
  ]
}
```

**Honeypot Response (Log and Ignore)**:

```json
{
  "actions": [
    {
      "type": "update_instruction",
      "instruction": "Log all ARP requests and ignore them"
    },
    {
      "type": "ignore_arp"
    }
  ]
}
```

## Connection Management

### No Connection State

ARP is stateless:

- Each ARP packet is independent event
- No handshake, no session
- No state persistence between packets

### Packet Processing Flow

1. pcap captures ARP packet from interface (BPF filter="arp")
2. Parse Ethernet frame, extract ARP packet
3. Parse ARP packet (operation, sender/target MAC/IP)
4. Create `arp_request_received` event with structured data
5. Spawn async task to call LLM
6. LLM analyzes and returns actions
7. If `send_arp_reply`: build and inject ARP reply frame
8. Loop continues for next ARP packet

### Concurrent Packet Processing

- Each ARP packet spawned in separate tokio task
- No queueing (unlike TCP protocols)
- Multiple ARP requests processed in parallel
- Ollama lock serializes LLM calls but not pcap capture

## Known Limitations

### 1. Requires Root/Admin Privileges

**Why**: libpcap needs a capture handle on the interface. There is no platform-specific code path
in this module - `pcap::Capture::open()` is the single privileged operation on every OS, but what
it needs differs:

- **Linux**: root, or `sudo setcap cap_net_raw,cap_net_admin+ep /path/to/netget`.
- **macOS / BSD**: root, **or** read/write access to `/dev/bpf*`. On a stock macOS those nodes are
  `crw------- root:wheel`, so unprivileged capture fails even for a user in the `access_bpf`
  group; Wireshark's *ChmodBPF* launch daemon is what normally relaxes them. Beware the trap that
  `pcap::Device::list()` **succeeds without any privilege** (it is just `getifaddrs`), so device
  enumeration is not evidence that capture will work - see the note at `src/privilege.rs:129-144`.
- **Windows**: `arp` is not built into the `dist-windows` feature set.

**Failure mode**: starting the capture is *not* fire-and-forget. `spawn_with_llm` awaits a
`oneshot` readiness signal from the blocking pcap task and returns `Err` if the handle could not be
opened, so `server_startup` records `ServerStatus::Error(..)`. An unprivileged MCP caller sees:

```
Failed to start server: failed to open pcap capture on 'en0' (needs root, or read access to
/dev/bpf* on macOS/BSD, or CAP_NET_RAW on Linux)
```

and a bad interface name gives `no such capture device 'nosuch0'`. The server is never reported as
`Running` while capturing nothing.

This is guarded by `tests/capture_startup_reports_failure_test.rs`, which covers all four
capture/raw-socket protocols (`arp`, `datalink`, `isis`, `icmp`) and asserts **both** branches -
privileged `spawn` returns `Ok`, unprivileged `spawn` returns `Err` naming the missing privilege.
Asserting the unprivileged branch is what gives it teeth: that is the branch every developer
machine and every CI runner takes, and a regression to fire-and-forget fails it immediately. The
same bug was fixed three separate times (ARP, DataLink, ICMP) and missed on IS-IS each time; the
test exists so a fourth recurrence is a red build rather than another audit finding.

**Default interface**: `default_binding()` resolves to `lo` on Linux/Windows and `lo0` on
macOS/BSD (`DEFAULT_LOOPBACK_INTERFACE` in `actions.rs`). Note ARP is not observable on loopback -
pass a real NIC (`en0`, `eth0`) via the `interface` argument for anything useful.

**Test Impact**: E2E tests must run with privileges.

### 2. Same Segment Only

- ARP is Layer 2 protocol (doesn't cross routers)
- Only works on local network segment
- Can't respond to ARP from different subnets
- Routers don't forward ARP requests

**Use Case Limitation**: Honeypot limited to local segment.

### 3. No ARP Cache Manipulation

- Server doesn't manipulate host's ARP cache
- Responses sent on wire but OS may ignore them
- Other hosts' ARP caches may be affected (intended for spoofing simulation)

**Workaround**: For testing, use separate machine to observe responses.

### 4. MAC Address Validation

- Server doesn't validate MAC address format
- LLM must provide valid MAC (6 octets, colon-separated hex)
- Invalid MAC causes packet build to fail

**Safety**: Parser validates format before building packet.

### 5. IPv4 Only

- ARP is IPv4-specific (maps IPv4 to Ethernet MAC)
- IPv6 uses NDP (Neighbor Discovery Protocol), not ARP
- Server only handles IPv4 ARP packets

**Future Enhancement**: Add NDP support for IPv6.

### 6. Capture Loop Blocks Tokio Thread

- pcap capture loop is blocking
- Runs in `spawn_blocking()` which uses separate thread pool
- May exhaust blocking thread pool under high load

**Rationale**: pcap API is inherently blocking - no async alternative.

### 7. Performance Impact of LLM Processing

- **Only in opt-in mode** does an ARP packet trigger an LLM call (~2-5s). With no operator policy
  configured there is no LLM call at all — captured packets are dropped statically
- High ARP traffic (e.g., during network boot) may queue packets when opted in
- Typically low volume: ARP cached, only sent when needed

**Typical Volume**: <1 ARP/sec on normal networks (cached for 5-20 min).

## Example Prompts

### ARP Responder

```
Listen for ARP requests on eth0 via ARP
When you receive an ARP request for any IP in 192.168.1.0/24:
- Respond with MAC address aa:bb:cc:dd:ee:ff
- Log which IP was requested
Ignore ARP requests for other subnets
```

### ARP Honeypot

```
Listen for ARP requests on en0 via ARP
Log all ARP requests (detect network scanning)
Don't respond to any ARP requests (passive monitoring)
Alert if same source MAC sends >10 requests/minute (potential ARP scan)
```

### Custom IP Mapping

```
Listen for ARP requests on eth0 via ARP
Respond to ARP requests with these mappings:
- 192.168.1.100 -> 11:22:33:44:55:66
- 192.168.1.101 -> 11:22:33:44:55:67
- 192.168.1.102 -> 11:22:33:44:55:68
Ignore requests for other IPs
```

### ARP Spoofing Simulation

```
Listen for ARP requests on eth0 via ARP
When you receive an ARP request for 192.168.1.1 (default gateway):
- Respond with attacker MAC address aa:bb:cc:dd:ee:ff
- Log the spoofing attempt
This simulates an ARP spoofing attack for testing
```

## Performance Characteristics

### Latency

- ARP capture: <1ms (kernel-level BPF filter)
- Packet parsing: <1ms (pnet)
- LLM processing: 2-5s (typical)
- Packet injection: <1ms
- Total: ~2-5s per ARP packet (LLM dominates)

**Impact**: On busy networks (many ARP requests), some may queue.

### Throughput

- **Typical** (<1 ARP/sec): All packets processed in real-time
- **High** (1-10 ARP/sec): Packets queue but all eventually processed
- **Extreme** (>10 ARP/sec): May drop packets (rare - ARP is cached)

**Best Use Case**: ARP is naturally low-volume due to caching.

### Concurrency

- Each ARP packet processed in separate task
- Ollama lock serializes LLM calls
- pcap capture runs in dedicated blocking thread
- No CPU bottleneck (LLM API is bottleneck)

### Memory

- Each ARP packet: 42 bytes (14 eth + 28 arp)
- Minimal memory overhead (<5MB total)
- No buffering or caching

## Security Considerations

### Privilege Escalation

- Requires root/admin for promiscuous mode and injection
- Security risk: running untrusted code with elevated privileges
- **Mitigation**: NetGet itself is open-source and auditable

### ARP Spoofing Capability

- Server can send fake ARP replies (by design)
- Could be used maliciously to intercept traffic
- **Intended Use**: Testing, education, honeypots only
- **Ethics**: Only use on networks you own or have permission to test

### Network Disruption

- Sending invalid ARP replies can disrupt network connectivity
- Hosts may cache incorrect MAC addresses
- **Mitigation**: Use on isolated test networks

### Honeypot Usage

ARP server is excellent for honeypots:

- Detect ARP reconnaissance (who's probing what IPs?)
- Log ARP scan patterns
- Identify attacker MAC addresses
- Monitor lateral movement attempts

## Use Cases

### 1. ARP Monitoring

- Detect ARP spoofing attacks (unexpected replies)
- Monitor ARP cache behavior
- Track IP-to-MAC mappings
- Identify network topology changes

### 2. Honeypot Operations

- Detect reconnaissance (ARP scanning for live hosts)
- Log attack traffic (which IPs are being probed?)
- Identify attacker devices (MAC addresses)
- Simulate fake devices (respond to ARP as if devices exist)

### 3. Network Testing

- Test ARP behavior of devices
- Simulate network conditions (duplicate IP, MAC changes)
- Debug ARP issues
- Experiment with custom mappings

### 4. Educational

- Learn ARP protocol mechanics
- Understand Layer 2 addressing
- Study ARP spoofing attacks
- Experiment with network protocols

### 5. Custom Network Configurations

- Virtual IP addresses (respond to ARP for non-existent IPs)
- Load balancing (multiple MACs for one IP)
- Migration (old IP pointing to new MAC)

## References

- [RFC 826 - Address Resolution Protocol](https://datatracker.ietf.org/doc/html/rfc826)
- [libpcap Documentation](https://www.tcpdump.org/manpages/pcap.3pcap.html)
- [pcap crate Documentation](https://docs.rs/pcap/latest/pcap/)
- [pnet crate Documentation](https://docs.rs/pnet/latest/pnet/)
- [ARP Spoofing Explained](https://en.wikipedia.org/wiki/ARP_spoofing)
- [Wireshark ARP Filter](https://wiki.wireshark.org/AddressResolutionProtocol)
