# IS-IS Client Implementation

## Overview

The IS-IS (Intermediate System to Intermediate System) client captures and analyzes IS-IS routing protocol PDUs at Layer
2. IS-IS is a link-state routing protocol that operates directly on top of the data link layer using LLC/SNAP
encapsulation.

## Protocol Background

**IS-IS Protocol:**

- **Layer**: Layer 2 (Data Link) - Uses LLC/SNAP, not IP
- **Standard**: ISO/IEC 10589
- **Encapsulation**: Ethernet + LLC/SNAP + IS-IS PDU
- **NLPID**: 0x83 (Network Layer Protocol ID for IS-IS)

**PDU Types:**

- **Hello PDUs**: Level 1 LAN Hello (15), Level 2 LAN Hello (16), P2P Hello (17)
- **Link State PDUs (LSP)**: Level 1 LSP (18), Level 2 LSP (20)
- **CSNP**: Complete Sequence Number PDUs - Level 1 (24), Level 2 (25)
- **PSNP**: Partial Sequence Number PDUs - Level 1 (26), Level 2 (27)

## Library Choices

**pcap (libpcap bindings):**

- **Purpose**: Raw packet capture at Layer 2
- **Rationale**: IS-IS operates at Layer 2, requires raw frame capture
- **Limitations**: Requires root/CAP_NET_RAW privileges, platform-dependent

**Custom Parsing:**

- **Rationale**: No mature Rust library for IS-IS protocol parsing
- **Implementation**: Basic PDU header parsing (type, version, length)
- **Limitations**: Only parses header, not full TLV (Type-Length-Value) structure

## Architecture

### Connection Model

**Passive Capture:**

- IS-IS client is **passive only** (capture mode, not active participation)
- Opens network interface in promiscuous mode
- Filters for IS-IS traffic (LLC/SNAP with DSAP/SSAP 0xFE)
- Captures and parses IS-IS PDUs

**Blocking Operation:**

- pcap is blocking I/O
- Runs in `tokio::task::spawn_blocking` to avoid blocking async runtime
- Uses sync methods (`update_client_status_sync`, `get_instruction_for_client_sync`)

### Packet Structure

```
Ethernet Frame:
[Dest MAC (6)] [Src MAC (6)] [EtherType (2)]

LLC/SNAP Header (8 bytes):
[DSAP (1)] [SSAP (1)] [Control (1)] [OUI (3)] [PID (2)]

IS-IS PDU:
[Intradomain Routing Protocol Discriminator (0x83)]
[Length Indicator]
[Version/Protocol ID Extension]
[ID Length]
[PDU Type]
[Version]
[Reserved]
[Maximum Area Addresses]
... (rest of PDU)
```

**Offsets:**

- Ethernet header: 0-13 (14 bytes)
- LLC/SNAP header: 14-21 (8 bytes)
- IS-IS PDU: 22+ (variable length)

### State Machine

**Capture Loop:**

1. Open network interface with pcap
2. Apply filter for IS-IS traffic
3. Capture packets in loop:
    - Parse Ethernet frame
    - Skip Ethernet + LLC/SNAP headers
    - Parse IS-IS PDU header
    - Call LLM with PDU event
    - Update memory with topology information
4. Continue until stop requested

**No Connection State:**

- Unlike TCP clients, IS-IS client has no connection state
- No Idle/Processing/Accumulating states (each PDU is independent)
- LLM memory tracks topology across multiple PDUs

## LLM Integration

### Event Triggers

**isis_pdu_received:**

- Triggered when any IS-IS PDU is captured
- Provides PDU type, version, length, and raw hex data
- LLM can analyze topology, neighbor relationships, link states

### Actions

**Async Actions (User-triggered):**

- `analyze_topology`: Request analysis of captured topology data
- `stop_capture`: Stop capturing IS-IS PDUs

**Sync Actions (Response to PDU):**

- `wait_for_more`: Continue capturing (default behavior)

**Note**: IS-IS client is passive - it only captures and analyzes, does not send PDUs.

### LLM Capabilities

**Topology Analysis:**

- Identify routers by System ID from LSPs
- Map neighbor relationships from Hello PDUs
- Analyze link costs and metrics
- Track network topology changes over time

**Use Cases:**

- Network topology discovery
- IS-IS debugging and troubleshooting
- Routing protocol monitoring
- Network visualization

### Dashboard injection (`[ send ]`)

`connect_with_llm_actions` registers a command channel
(`client::command_support::register_command_channel`) before the pcap task starts and spawns
a `command_loop` task (registered with `register_client_task`). This client makes no
connected-event LLM call, so there is no park to race.

**This client can never report `Sent`, and that is the honest answer, not a gap in the
wiring.** The IS-IS client is a receive-only sniffer: it opens a capture and never a transmit
path, so no injected action can put a PDU on the wire. Injected actions are still executed
through the protocol's own `execute_action`, exactly as LLM-produced ones are:

| Outcome | When |
|---|---|
| `Executed { detail }` | `analyze_topology` / `wait_for_more` - `detail` says the client is receive-only and transmits nothing |
| `Rejected { error }` | any verb the protocol does not define (including anything that would "send" a PDU) |
| `Disconnected` | an injected `stop_capture` |

`stop_capture` is real work: the command loop marks the client disconnected, and the capture
loop checks that status on every iteration, so the capture stops within one pcap timeout
(1s). The handle is dropped there and when the capture loop exits.

### Stopping the capture — two paths, and only one of them is a status change

`stop_capture` works by *setting* `ClientStatus::Disconnected`, which the loop sees. Removing
the client does not: `AppState::remove_client` **deletes the entry** (`inner.clients.remove`),
so `get_client` returns `None` and a liveness poll that only inspects `Some(client)` never
fires. Nor can the blocking task simply be aborted — Tokio cannot unwind a thread parked in
`next_packet()`, which is why `crate::utils::StopSignal` exists.

So the capture is stopped by a `StopSignal` the loop polls once per iteration, whose
`park_task()` is what gets handed to `register_client_task`. `remove_client` aborts that task,
its guard's `Drop` trips the flag, and the loop exits at its next poll — within ~1s, because
the capture is opened with `.timeout(1000)`. The blocking task's own `JoinHandle` is
deliberately *not* registered, since aborting it would achieve nothing. This mirrors
`crate::server::isis` exactly; the server had it and the client did not.

**Do not remove the pcap read timeout** — a poll between packets only runs when the blocking
call returns, so without it shutdown becomes unbounded on an idle interface.

Test: `tests/client/isis/command_channel_test.rs` (zero LLM calls, privilege-independent -
none of these outcomes touches the pcap handle).

## Limitations

### Implementation Limitations

1. **Root Access Required**: pcap requires root privileges or CAP_NET_RAW capability
2. **Passive Only**: Cannot send IS-IS PDUs (would require full router implementation)
3. **Basic Parsing**: Only parses PDU header, not full TLV structure
4. **No Database**: Does not maintain link-state database (relies on LLM memory)
5. **Platform Dependent**: pcap behavior varies across operating systems

### Protocol Limitations

1. **Layer 2 Only**: Cannot capture IS-IS over point-to-point links (PPP, HDLC)
2. **No Authentication**: Cannot parse or validate IS-IS authentication TLVs
3. **No LSP Reassembly**: Large LSPs fragmented across multiple PDUs not reassembled
4. **Memory Constraints**: LLM memory may not scale to large topologies (100+ routers)

### Security Considerations

1. **Promiscuous Mode**: Captures all traffic on the interface (privacy concern)
2. **Root Required**: Running as root increases attack surface
3. **No Validation**: Does not validate PDU checksums or signatures
4. **Passive Only**: Cannot inject malicious PDUs (good security property)

## Testing Considerations

**Test Environment:**

- Requires IS-IS router or packet replay
- Test on isolated network (avoid production networks)
- Use virtual interfaces (veth pairs) for safe testing

**Test Approach:**

- Use FRRouting (FRR) or Bird routing daemon for IS-IS
- Or replay captured IS-IS traffic with `tcpreplay`
- Verify PDU parsing and LLM analysis

**Known Issues:**

- May miss PDUs in high-traffic environments (pcap buffer overflow)
- Platform-specific pcap filter syntax variations

## Example Prompts

```
Capture IS-IS PDUs on eth0 and show me the network topology
```

```
Monitor IS-IS on interface en0 and alert me if any routers disappear
```

```
Analyze IS-IS Hello PDUs on wlan0 to identify all neighbors
```

## Future Enhancements

1. **Full TLV Parsing**: Parse all TLV types (Area Address, IS Neighbors, IP Reachability, etc.)
2. **LSP Database**: Maintain in-memory link-state database
3. **Topology Visualization**: Export topology as graph (DOT format)
4. **Multi-Level Support**: Distinguish Level 1 vs Level 2 topologies
5. **Authentication**: Parse and validate authentication TLVs
6. **Active Mode**: Implement basic Hello/LSP sending (requires full router stack)

## References

- ISO/IEC 10589: Information technology — Telecommunications and information exchange between systems — Intermediate
  System to Intermediate System intra-domain routing information exchange protocol
- RFC 1142: OSI IS-IS Intra-domain Routing Protocol
- RFC 5308: Routing IPv6 with IS-IS
