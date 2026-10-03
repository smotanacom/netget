# IS-IS Server Implementation

## Overview

IS-IS (Intermediate System to Intermediate System) routing protocol server implementing ISO/IEC 10589 and RFC 1195 (
IS-IS for IP).

### Default behaviour: passive listener, no LLM

Whether to respond is **policy**, not something a received PDU determines. So with **no operator
policy** (no server instruction and no per-event handler), the server is a **passive listener — no
response, no LLM call** per captured PDU (gated by `should_call_llm` in `mod.rs` =
`has_instruction || has_handler`). The model is consulted **only when the operator opts in** with
how the router should behave (an instruction or a handler). This passive default is
compile-verified since the pcap path needs root and the E2E suite is `#[ignore]`d. LSP/CSNP/PSNP
are parsed and logged but not handled; there is no LSP database or SPF. The `LLM Integration`
material below (neighbor adjacencies, Hello responses, LSP generation) is the opt-in path.

**Status**: Experimental
**Protocol Spec
**: [ISO/IEC 10589](https://www.iso.org/standard/30932.html), [RFC 1195 (IS-IS for IP)](https://datatracker.ietf.org/doc/html/rfc1195)
**Transport**: raw Layer 2 via libpcap - Ethernet 802.3 + LLC (DSAP/SSAP `0xFE`, control `0x03`).
**There is no port and no UDP.** Everything in this file that used to describe "UDP encapsulation
on port 3784" was left over from a draft that does not exist in `mod.rs`; the `stack_name()` said
`ETH>IP>UDP>ISIS` for the same reason and now says `ETH>LLC>ISIS`.
**Privilege**: `PrivilegeRequirement::PacketCapture` - root, `/dev/bpf*` access on macOS/BSD, or
`CAP_NET_RAW` on Linux.

## Library Choices

### No Library - Manual Protocol Implementation

**Why no library**:

- Holo exists but is a full production routing daemon, not a library
- No lightweight IS-IS parsing library available
- IS-IS packet structure is well-documented (ISO/IEC 10589, RFC 1195)
- Manual implementation provides full LLM control over routing behavior
- Honeypot/learning use case doesn't require full routing functionality

**What we implement manually**:

- IS-IS PDU parsing (Hello, LSP, CSNP, PSNP)
- TLV (Type-Length-Value) encoding/decoding
- Hello PDU construction with area addresses
- Simplified LSP construction
- Adjacency state tracking

**Why not alternatives**:

- `holo` - Production routing suite, too heavyweight for NetGet
- No other Rust IS-IS libraries exist
- External daemon (FRR, BIRD) - Violates NetGet architecture

## Architecture Decisions

### Layer 2 over libpcap (not UDP)

IS-IS runs directly on the data link layer, and so does this implementation:

- **Capture**: `pcap::Capture::from_device(..).promisc(true).snaplen(65535).timeout(1000).open()`
- **BPF filter**: `ether proto 0xfefe or (ether[14:2] = 0xfefe and ether[16:1] = 0x03)`
- **Framing**: 14-byte Ethernet header, 3-byte LLC (`0xFE 0xFE 0x03`), then the IS-IS PDU at
  offset 17. Replies are built by `build_ethernet_frame()` and injected with `sendpacket()`.
- **Destination MAC**: `01:80:C2:00:00:14` (AllL1ISs) or `01:80:C2:00:00:15` (AllL2ISs),
  chosen from the `level` startup parameter.
- **Consequence**: elevated privileges are required, and loopback is useless for real IS-IS
  (there are no IS-IS speakers on it) - point it at a real NIC or a veth pair.

### Startup reports failure (do not regress this)

`spawn_with_llm_actions` opens the pcap handle inside `tokio::task::spawn_blocking` - but it
**awaits the outcome over a `oneshot` before returning**, so device lookup, the privileged
`open()` and the BPF filter compile all propagate out of `Server::spawn()` and `server_startup`
records `ServerStatus::Error(..)`.

This was not always so. IS-IS shipped the fire-and-forget version: the blocking task logged and
returned while `spawn` had already committed to `Ok(interface)`, so an unprivileged user got a
server sitting in `Running` that had captured nothing. ARP, DataLink and ICMP each had the same
bug and were each fixed separately; IS-IS was missed all three times. An unprivileged caller now
sees:

```
Failed to start server: failed to open pcap capture on 'en0' (needs root, or read access to
/dev/bpf* on macOS/BSD, or CAP_NET_RAW on Linux)
```

and a bad interface name gives `no such capture device 'nosuch0'`.

The BPF filter is deliberately fatal too: the expression is fixed, so a failure to compile it
means the capture would deliver *every* frame on the segment to the LLM. Refusing to start beats
becoming a firehose.

`tests/capture_startup_reports_failure_test.rs` guards all four protocols and asserts both
branches (privileged → `Ok`, unprivileged → `Err` naming the missing privilege), so it has teeth
on an unprivileged runner - which is every developer machine and every CI runner.

### IS-IS PDU Types

IS-IS uses 4 types of PDUs:

1. **Hello (IIH)** - Neighbor discovery and adjacency maintenance
    - LAN Hello Level 1 (type 15)
    - LAN Hello Level 2 (type 16)
    - Point-to-Point Hello (type 17)

2. **LSP (Link State PDU)** - Topology information distribution
    - Level 1 LSP (type 18)
    - Level 2 LSP (type 20)

3. **CSNP (Complete Sequence Number PDU)** - Database synchronization
    - Level 1 CSNP (type 24)
    - Level 2 CSNP (type 25)

4. **PSNP (Partial Sequence Number PDU)** - LSP acknowledgment/request
    - Level 1 PSNP (type 26)
    - Level 2 PSNP (type 27)

**Current implementation**: Hello and LSP parsing/construction. CSNP/PSNP logged but not handled.

### TLV (Type-Length-Value) Encoding

IS-IS uses TLV encoding for extensibility. Each TLV has:

- Type (1 byte) - identifies the TLV
- Length (1 byte) - length of value field
- Value (variable) - data

**Common TLVs implemented**:

- Area Addresses (type 1) - IS-IS area IDs
- Protocols Supported (type 129) - IPv4/IPv6 support
- IP Interface Addresses (type 132) - IPv4 addresses
- Hostname (type 137) - Router hostname

### IS-IS Addressing

**System ID**: 6-byte identifier (e.g., `0000.0000.0001`)

- Uniquely identifies an IS-IS router
- Format: 3 dotted groups of 4 hex digits

**Area ID**: Variable-length (e.g., `49.0001`)

- Identifies the IS-IS area
- Private areas start with 49 (similar to RFC 1918)

**Level**: IS-IS routers operate at one or both levels

- Level 1: Intra-area routing
- Level 2: Inter-area routing (backbone)
- Level 1+2: Both levels

### Adjacency State Machine

Simplified IS-IS adjacency states:

1. **Init**: Received Hello, neighbor detected
2. **Up**: Adjacency established, can exchange routing info
3. **Down**: Adjacency lost (holding time expired)

Full IS-IS has more complex state machine, but this is sufficient for honeypot/testing.

## LLM Integration

### Startup Parameters

Server configured with:

```json
{
  "system_id": "0000.0000.0001",
  "area_id": "49.0001",
  "level": "level-2"
}
```

Extracted from LLM-generated startup prompt.

### Sync Actions (Network event triggered)

1. **send_isis_hello**: Send Hello PDU for neighbor discovery
    - `pdu_type`: "lan_hello_l1", "lan_hello_l2", or "p2p_hello"
    - `system_id`: Local system ID (e.g., "0000.0000.0001")
    - `area_id`: Area ID (e.g., "49.0001")
    - `holding_time`: Holding time in seconds (default: 30)

2. **send_isis_lsp**: Send Link State PDU
    - `level`: "level-1" or "level-2"
    - `system_id`: Local system ID

3. **send_isis_pdu**: Send raw IS-IS PDU from hex
    - `data`: Hex-encoded PDU

4. **ignore_pdu**: No response to received PDU

### Event Types

**`isis_hello`** - IS-IS Hello PDU received

Event data, as actually built in `handle_hello_pdu` - there is no `peer_addr` and no
`pdu_type_code`, because there is no IP peer; the neighbour is identified by MAC:

```json
{
  "pdu_type": "LAN Hello L2",
  "src_mac": "aa:bb:cc:dd:ee:ff",
  "packet_hex": "831b01...",
  "area_addresses": ["490001"],
  "protocols_supported": ["0xcc"],
  "ip_addresses": ["192.168.1.100"],
  "hostname": "router1"
}
```

The last four keys are present only when the corresponding TLV appeared in the Hello.

## Connection Management

### Per-PDU "Connection"

Each captured PDU creates a "connection" entry so the TUI has something to show:

- Connection ID: unique per PDU (`get_next_unified_id()`)
- `remote_addr` / `local_addr` are the source and local **MAC** rendered into a `SocketAddr`-
  shaped string, which does not parse, so both fall back to `0.0.0.0:0`. Cosmetic, but do not
  read those fields as meaningful.
- `protocol_info` is `ProtocolConnectionInfo::empty()`. There is **no** `ProtocolConnectionInfo::Isis`
  variant - `ProtocolConnectionInfo` is a generic `serde_json::Value` wrapper, not an enum, and
  no adjacency state is stored anywhere.

## Logging

### Dual Logging Strategy

**DEBUG**: PDU summaries

- "IS-IS received 145 bytes from aa:bb:cc:dd:ee:ff"
- "IS-IS sent 96 bytes"

**TRACE**: Full packet dumps

- "IS-IS PDU (hex): 831b01..."
- "IS-IS sent (hex): 831b01..."

**INFO**: Adjacency events

- "IS-IS LAN Hello L2 from aa:bb:cc:dd:ee:ff"
- "IS-IS LSP received (forwarding to LLM)"

**WARN**: Invalid packets

- "IS-IS invalid protocol discriminator: 0x82"
- "IS-IS unsupported version: 2"

All logs go to both `netget.log` (via tracing macros) and TUI (via `status_tx`).

## Limitations

### Partial Implementation

**Implemented**:

- ✅ Hello PDU parsing (all 3 types)
- ✅ Hello PDU construction
- ✅ TLV parsing (Area, Protocols, IP Addresses, Hostname)
- ✅ Basic LSP construction

**Verified against**: nothing. No IS-IS speaker, no independent codec, no captured trace. The
`metadata()` note claimed "interoperable with real routers (FRR, Cisco, etc.)"; that was never
tested and has been removed. What *is* tested is the startup contract
(`tests/capture_startup_reports_failure_test.rs`) and PDU-structure constants
(`tests/server/isis/e2e_test.rs::test_isis_pdu_structure`); every test that would exercise a real
frame is `#[ignore]`d for root.

**Not Implemented**:

- ❌ CSNP/PSNP handling (logged but ignored)
- ❌ LSP database (no topology storage)
- ❌ SPF (Shortest Path First) calculation
- ❌ Routing table integration
- ❌ Designated IS election (for LAN segments)
- ❌ Authentication (MD5, HMAC-SHA)
- ❌ IPv6 support (only IPv4 TLVs)
- ❌ Flooding logic
- ❌ Holding time enforcement
- ❌ Adjacency state tracking (the event carries no state and nothing is stored between PDUs;
  the "Adjacency State Machine" section above describes an intent, not the code)

### No Routing Functionality

Server doesn't perform routing:

- LSPs parsed but not stored in database
- No SPF calculation or route computation
- Cannot participate in real IS-IS networks
- For honeypot, testing, and learning only

### Requires capture privileges, and a segment with an IS-IS speaker on it

Framing is real Layer 2 with the correct AllL1ISs/AllL2ISs multicast destinations, so nothing
structural prevents interoperation - but it has never been demonstrated. Practical consequences:

- Needs root, `/dev/bpf*` access (macOS/BSD) or `CAP_NET_RAW` (Linux); startup refuses without.
- Loopback is pointless: no router speaks IS-IS there. Use a real NIC or a veth pair.
- `get_interface_mac()` only reads `/sys/class/net/<if>/address`, i.e. Linux. Everywhere else it
  fails and the server sends from the locally administered `02:00:00:00:00:01`.

### No Multi-Level Support

Each server instance operates at a single level (L1 or L2):

- No Level 1+2 router support
- No inter-level route leaking
- Simplified for testing

## Examples

### Server Startup

```
netget> Start an IS-IS router on interface eth0 with system-id 0000.0000.0001 in area 49.0001
```

Server output:

```
[INFO] IS-IS server starting on interface: eth0
[INFO] IS-IS configured: system_id=0000.0000.0001, area=49.0001, level=level-2
→ IS-IS ready on interface eth0
[INFO] IS-IS capture active on eth0
```

Without capture privileges it refuses instead, and the server is marked `Error`:

```
[ERROR] IS-IS capture startup failed: failed to open pcap capture on 'eth0' (needs root, or
read access to /dev/bpf* on macOS/BSD, or CAP_NET_RAW on Linux)
```

### Hello PDU Received

Client sends Hello:

```
[DEBUG] IS-IS received 145 bytes
[TRACE] IS-IS frame (hex): 0180c2000015aabbccddeeff...
→ IS-IS LAN Hello L2 from aa:bb:cc:dd:ee:ff
```

LLM receives event:

```json
{
  "pdu_type": "LAN Hello L2",
  "src_mac": "aa:bb:cc:dd:ee:ff",
  "packet_hex": "831b01001001...",
  "area_addresses": ["490001"],
  "protocols_supported": ["0xcc"],
  "hostname": "neighbor-router"
}
```

LLM responds:

```json
{
  "actions": [
    {
      "type": "send_isis_hello",
      "pdu_type": "lan_hello_l2",
      "system_id": "0000.0000.0001",
      "area_id": "49.0001",
      "holding_time": 30
    }
  ]
}
```

Server sends Hello:

```
[DEBUG] IS-IS sent 96 bytes
[TRACE] IS-IS sent (hex): 831b0100100106000...
```

### LSP Received

```
[INFO] IS-IS LSP received (forwarding to LLM)
```

Currently logged but not fully processed (no LSP database).

## Use Cases

### Learning IS-IS Protocol

- Understand IS-IS packet structure
- Experiment with Hello PDUs and adjacencies
- Analyze TLV encoding
- Study routing protocol behavior

### Honeypot/Monitoring

- Detect unauthorized IS-IS routers on network
- Log IS-IS reconnaissance attempts
- Monitor for routing protocol attacks
- Simulate IS-IS router for testing

### Testing

- Test IS-IS client implementations
- Validate IS-IS packet parsing
- Simulate network topologies
- Debug IS-IS issues

### NOT for Production Routing

IS-IS server should **not** be used for production routing:

- No SPF calculation or routing table
- No LSP database or flooding
- Never validated against a real IS-IS speaker
- No authentication or security features

For production routing, use established implementations (FRR, BIRD, Holo).

## References

- [ISO/IEC 10589 - IS-IS Routing Protocol](https://www.iso.org/standard/30932.html)
- [RFC 1195 - Use of OSI IS-IS for Routing in TCP/IP and Dual Environments](https://datatracker.ietf.org/doc/html/rfc1195)
- [RFC 5120 - M-ISIS: Multi Topology Routing in IS-IS](https://datatracker.ietf.org/doc/html/rfc5120)
- [RFC 5303 - Three-Way Handshake for IS-IS Point-to-Point Adjacencies](https://datatracker.ietf.org/doc/html/rfc5303)
- [RFC 5305 - IS-IS Extensions for Traffic Engineering](https://datatracker.ietf.org/doc/html/rfc5305)
- [Holo Routing Suite](https://github.com/holo-routing/holo) - Production IS-IS implementation in Rust
