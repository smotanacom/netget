# ARP Client Implementation

## Overview

Layer 2 Address Resolution Protocol (ARP) client that sends ARP requests and monitors ARP traffic using libpcap and
pnet. Allows the LLM to send ARP queries (who-has), gratuitous ARP announcements, and analyze ARP responses for network
reconnaissance and testing.

**Status**: Experimental (Layer 2 Client Protocol)
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
    - Packet injection via `sendpacket()` for ARP requests/replies

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
- Shares same dependencies as ARP server for consistency

## Architecture Decisions

### 1. Blocking I/O with Tokio Bridge

pcap has blocking API, Tokio is async:

**Solution**:

- Spawn packet capture in `tokio::task::spawn_blocking()`
- Blocking task runs pcap capture loop with ARP filter
- For each ARP packet received, spawn async task via `runtime.spawn()` for LLM processing
- LLM decides how to respond (send ARP request/reply, wait, etc.)
- Actions sent via channel to packet injection thread

**Tradeoff**: Extra task spawning overhead, but necessary for pcap API compatibility.

### 2. Full ARP Packet Injection

ARP client can both capture and inject packets:

- LLM receives ARP packet events (requests or replies from other hosts)
- LLM can send `send_arp_request` action (who-has query)
- LLM can send `send_arp_reply` action (gratuitous ARP or response)
- Actions specify sender/target MAC/IP addresses
- Client builds complete Ethernet frame with ARP packet
- Frame injected via pcap's `sendpacket()`

**Use Cases**:

1. Network reconnaissance (send who-has queries for IP ranges)
2. Gratuitous ARP announcements (announce own MAC for an IP)
3. ARP monitoring (passive capture of ARP traffic)
4. ARP testing (test how hosts respond to ARP packets)

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

Client requires network interface as startup parameter:

- Example: "eth0", "en0", "wlan0"
- Specified in `remote_addr` field when opening client
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
- Essential for network monitoring and reconnaissance
- Allows detection of ARP requests/replies between other hosts

**Security**: Only works on same network segment (same switch/hub). Can't capture ARP on other segments.

### 7. State Machine for LLM Processing

Uses a two-state machine (Idle → Processing → Idle). There is **no `Accumulating` state and
no queue** — `ConnectionState` in `mod.rs` has exactly `Idle` and `Processing`, and a packet
that arrives while the model is thinking is **dropped**, with a `debug!` line saying so. That
is acceptable for ARP (it is naturally low-volume, and a dropped observation is not a dropped
answer) but it is not the queueing machine `src/server/tcp/` implements, and this file used to
claim it was.

**Rationale**: Prevents concurrent LLM calls on the same client.

### 8. Dual Logging

All operations use **dual logging**:

- **DEBUG**: ARP summary (operation, sender, target)
- **TRACE**: Full packet hex dump
- **INFO**: LLM messages and high-level analysis
- **ERROR**: Capture errors, pcap failures, permission issues
- All logs go to both `netget.log` (via tracing) and TUI Status panel (via status_tx)

## LLM Integration

### Action-Based Request Model

The LLM controls ARP client with actions:

**Events**:

- `arp_client_started` - Client successfully started on interface
    - Parameters: `interface`
- `arp_response_received` - ARP packet captured from interface
    - Parameters: `operation`, `sender_mac`, `sender_ip`, `target_mac`, `target_ip`

**Available Actions (Async - User-triggered)**:

- `send_arp_request` - Send ARP request (who-has query)
    - Parameters: `sender_mac`, `sender_ip`, `target_ip`
- `send_arp_reply` - Send ARP reply (gratuitous ARP)
    - Parameters: `sender_mac`, `sender_ip`, `target_mac`, `target_ip`
- `stop_capture` - Stop ARP capture and close client

**Available Actions (Sync - Response to events)**:

- `send_arp_request` - Send ARP request in response to received packet
- `send_arp_reply` - Send ARP reply in response to received packet
- `wait_for_more` - Continue monitoring for ARP packets

### Example LLM Workflows

**Send ARP Request (Who-Has Query)**:

```json
{
  "actions": [
    {
      "type": "send_arp_request",
      "sender_mac": "aa:bb:cc:dd:ee:ff",
      "sender_ip": "192.168.1.100",
      "target_ip": "192.168.1.1"
    }
  ]
}
```

**Send Gratuitous ARP**:

```json
{
  "actions": [
    {
      "type": "send_arp_reply",
      "sender_mac": "aa:bb:cc:dd:ee:ff",
      "sender_ip": "192.168.1.100",
      "target_mac": "ff:ff:ff:ff:ff:ff",
      "target_ip": "192.168.1.100"
    }
  ]
}
```

**Monitor ARP and Respond to Specific IP**:

```json
{
  "actions": [
    {
      "type": "send_arp_reply",
      "sender_mac": "aa:bb:cc:dd:ee:ff",
      "sender_ip": "192.168.1.10",
      "target_mac": "11:22:33:44:55:66",
      "target_ip": "192.168.1.1"
    }
  ]
}
```

**Continue Monitoring (Wait for More)**:

```json
{
  "actions": [
    {
      "type": "wait_for_more"
    }
  ]
}
```

## Connection Management

### No Traditional Connection

ARP is stateless at the protocol level:

- No handshake, no session
- Each ARP packet is independent event
- Client remains active until explicitly stopped

### Client Lifecycle

1. User opens ARP client → Specify interface (e.g., "eth0")
2. Client starts packet capture on interface → Status: Connected
3. LLM receives `arp_client_started` event → Can send initial ARP requests
4. For each captured ARP packet → LLM receives `arp_response_received` event
5. LLM decides: send_arp_request, send_arp_reply, wait_for_more, or stop_capture
6. User closes client or LLM sends `stop_capture` → Status: Disconnected

### Packet Processing Flow

1. pcap captures ARP packet from interface (BPF filter="arp")
2. Parse Ethernet frame, extract ARP packet
3. Parse ARP packet (operation, sender/target MAC/IP)
4. Create `arp_response_received` event with structured data
5. Check state machine (Idle → Processing)
6. Spawn async task to call LLM
7. LLM analyzes and returns actions
8. If `send_arp_request` or `send_arp_reply`: build and inject ARP packet
9. Set state back to Idle
10. Loop continues for next ARP packet

### Concurrent Packet Processing

- Each ARP packet spawned in separate tokio task (if state is Idle)
- State machine prevents concurrent LLM calls for same client
- Packets received during Processing are dropped, not queued (see State Machine above)
- LLM concurrency is bounded by `--llm-max-concurrent` / `--llm-max-queued`

### Dashboard injection (`[ send ]`)

`start_with_llm_actions` registers a command channel
(`client::command_support::register_command_channel`) **before** the `arp_client_started` LLM
call, which a manual routing rule can park for minutes, and spawns a `command_loop` task
(registered with `register_client_task`).

There is no `AsyncWrite` half here, so the generic `handle_stream_client_command` cannot be
used. The frame-injection channel to the pcap thread is now created in the async function
rather than inside `spawn_blocking`, so the LLM path and an injected command hand frames to
**the same** dedicated injection thread through the same queue, and both build them with the
same `build_packet_for_custom_result`. Each queued frame carries an optional
`oneshot` acknowledgement, which is what makes a truthful outcome possible:

| Outcome | When |
|---|---|
| `Sent { bytes_sent }` | the injection thread acknowledged a successful `sendpacket` for that many bytes |
| `Executed { detail }` | the frame could not be handed over, or was not acknowledged - `detail` names the reason |
| `Rejected { error }` | unknown verb, or MAC/IP fields that cannot become a frame |
| `Disconnected` | an injected `stop_capture` |

**An unprivileged run has no client to inject into, deliberately.** libpcap needs root
(`/dev/bpf*` on macOS, `CAP_NET_RAW` on Linux). `start_with_llm_actions` awaits the pcap open
over a oneshot and returns `Err` if it fails, so `connect()` reports the failure — naming the
device and the privilege — and `client_startup` records `ClientStatus::Error`. It used to
return `Ok` regardless, which left the client sitting in `Connected` having opened nothing:
the client-side twin of the fire-and-forget `spawn_blocking` that left the ARP, DataLink and
ICMP *servers* `Running` with no capture.

There is no third option worth reaching for. `client_startup::connect_client` overwrites the
status with `ClientStatus::Connected` on **any** `Ok(..)`, so "return Ok but show an error" is
not expressible; the choice is `Err` or a lie. The `Executed { detail }` outcome remains for
the case the capture opened and later stopped.

**The started-event answer is executed, not counted.** Both LLM entry points — the
`arp_client_started` event and every captured packet — go through one `execute_client_actions`.
The started-event arm used to be `for _action in actions { debug!(..) }`: a client told to send
a gratuitous ARP announcement on start put nothing on the wire and reported success. That is
the defect the root `CLAUDE.md` describes under "clients that ask the model what to do and then
throw the answer away", and `tests/client_event_wiring_test.rs` now guards against its return.

**Shutdown**: the capture loop polls a `crate::utils::StopSignal` every iteration, and the
parked task registered with `register_client_task` trips it when the client is removed. The
pcap read timeout (1000ms) bounds how long that takes on an idle interface. Before this the
loop had no exit at all and kept capturing — and kept calling the LLM — until the process
exited.

Tests: `tests/client/arp/command_channel_test.rs` (zero LLM calls). It covers the startup
contract, the rejection paths, and — by driving `ArpClient::command_loop` directly with a
stand-in for the pcap injection thread — every outcome including `Sent { bytes_sent: 42 }`, all
unprivileged. `injected_arp_request_is_transmitted` is still `#[ignore]`d: it is the only thing
that proves libpcap itself accepted the frame, and that needs root.

## Known Limitations

### 1. Requires Root/Admin Privileges

**Why**: Promiscuous mode and packet injection require raw socket access

**Workaround**:

- Linux: Run as root or use `sudo setcap cap_net_raw+ep /path/to/netget`
- macOS: Run as root or use `sudo`
- Windows: Run as Administrator

**Test Impact**: E2E tests must run with privileges.

### 2. Same Segment Only

- ARP is Layer 2 protocol (doesn't cross routers)
- Only works on local network segment
- Can't send/receive ARP from different subnets
- Routers don't forward ARP requests

**Use Case Limitation**: Reconnaissance limited to local segment.

### 3. No ARP Cache Access

- Client doesn't read or manipulate host's ARP cache
- Can send ARP requests, but can't see OS ARP cache entries
- Responses captured from wire, not from local cache

**Workaround**: Use system tools like `arp -a` or `ip neigh` for cache inspection.

### 4. MAC Address Validation

- Client validates MAC address format before building packet
- LLM must provide valid MAC (6 octets, colon-separated hex)
- Invalid MAC causes packet build to fail with error

**Safety**: Parser validates format before building packet.

### 5. IPv4 Only

- ARP is IPv4-specific (maps IPv4 to Ethernet MAC)
- IPv6 uses NDP (Neighbor Discovery Protocol), not ARP
- Client only handles IPv4 ARP packets

**Future Enhancement**: Add NDP client support for IPv6.

### 6. Capture Loop Blocks Tokio Thread

- pcap capture loop is blocking
- Runs in `spawn_blocking()` which uses separate thread pool
- May exhaust blocking thread pool under extremely high load

**Rationale**: pcap API is inherently blocking - no async alternative.

### 7. Performance Impact of LLM Processing

- Each ARP packet triggers LLM call (~2-5s)
- High ARP traffic may queue packets
- Typically low volume: ARP cached, only sent when needed

**Typical Volume**: <1 ARP/sec on normal networks (cached for 5-20 min).

## Example Prompts

### ARP Network Scanner

```
Open ARP client on eth0
Send ARP requests for all IPs in 192.168.1.0/24
Log which IPs respond and their MAC addresses
```

### Gratuitous ARP Announcer

```
Open ARP client on en0
Send gratuitous ARP announcing MAC aa:bb:cc:dd:ee:ff for IP 192.168.1.100
This announces our presence on the network
```

### ARP Traffic Monitor

```
Open ARP client on eth0
Monitor all ARP traffic on the network
Log who is asking for what IPs
Don't send any ARP packets, just observe
```

### ARP Reconnaissance

```
Open ARP client on wlan0
Send who-has queries for 192.168.1.1, 192.168.1.10, 192.168.1.100
Log which IPs are alive based on ARP replies
```

## Performance Characteristics

### Latency

- ARP capture: <1ms (kernel-level BPF filter)
- Packet parsing: <1ms (pnet)
- LLM processing: 2-5s (typical)
- Packet injection: <1ms
- Total: ~2-5s per ARP packet (LLM dominates)

**Impact**: On busy networks (many ARP packets), some may queue.

### Throughput

- **Typical** (<1 ARP/sec): All packets processed in real-time
- **High** (1-10 ARP/sec): Packets queue but all eventually processed
- **Extreme** (>10 ARP/sec): May drop packets (rare - ARP is cached)

**Best Use Case**: ARP is naturally low-volume due to caching.

### Concurrency

- Each ARP packet processed in separate task (if state is Idle)
- LLM concurrency is bounded by `--llm-max-concurrent`
- pcap capture runs in dedicated blocking thread
- No CPU bottleneck (LLM API is bottleneck)

### Memory

- Each ARP packet: 42 bytes (14 eth + 28 arp)
- Minimal memory overhead (<5MB total)
- No buffering or caching beyond state machine

## Security Considerations

### Privilege Escalation

- Requires root/admin for promiscuous mode and injection
- Security risk: running untrusted code with elevated privileges
- **Mitigation**: NetGet itself is open-source and auditable

### ARP Spoofing Capability

- Client can send fake ARP packets (by design)
- Could be used maliciously to intercept traffic or disrupt networks
- **Intended Use**: Testing, education, network diagnostics only
- **Ethics**: Only use on networks you own or have permission to test

### Network Disruption

- Sending invalid or duplicate ARP packets can disrupt network connectivity
- Hosts may cache incorrect MAC addresses
- **Mitigation**: Use on isolated test networks

### Legitimate Use Cases

ARP client is designed for:

- Network diagnostics (check if host is reachable at Layer 2)
- ARP table debugging
- Network testing and education
- Security research (authorized environments only)

## Use Cases

### 1. Network Reconnaissance

- Discover live hosts on local segment
- Map IP-to-MAC address mappings
- Identify network topology
- Detect rogue devices

### 2. ARP Testing

- Test ARP behavior of devices
- Verify ARP cache timeout
- Test gratuitous ARP handling
- Debug ARP issues

### 3. Network Monitoring

- Monitor ARP traffic patterns
- Detect ARP spoofing attacks (unexpected replies)
- Track IP/MAC changes
- Identify network events

### 4. Educational

- Learn ARP protocol mechanics
- Understand Layer 2 addressing
- Experiment with network protocols
- Study ARP behavior in controlled environment

### 5. Network Diagnostics

- Check Layer 2 reachability
- Verify MAC address assignments
- Test switch/router ARP proxy behavior
- Debug connectivity issues

## References

- [RFC 826 - Address Resolution Protocol](https://datatracker.ietf.org/doc/html/rfc826)
- [libpcap Documentation](https://www.tcpdump.org/manpages/pcap.3pcap.html)
- [pcap crate Documentation](https://docs.rs/pcap/latest/pcap/)
- [pnet crate Documentation](https://docs.rs/pnet/latest/pnet/)
- [ARP Explained](https://en.wikipedia.org/wiki/Address_Resolution_Protocol)
- [Wireshark ARP Filter](https://wiki.wireshark.org/AddressResolutionProtocol)
