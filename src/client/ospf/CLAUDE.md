# OSPF Client Protocol

## Overview

**OSPF query client** that joins an OSPF network to monitor and query routers for topology information.

**Philosophy**: NetGet handles protocol details, LLM controls querying behavior

- ✅ Real OSPF protocol (IP 89)
- ✅ Multicast support (224.0.0.5)
- ✅ LLM queries routers
- ❌ NO real SPF calculation
- ❌ NO real routing table
- ❌ NO route installation

**Status**: Experimental (query/monitoring client)
**Spec**: [RFC 2328 (OSPFv2)](https://datatracker.ietf.org/doc/html/rfc2328)
**Requires**: Root/CAP_NET_RAW privileges

## Use Cases

### 1. OSPF Topology Discovery

Query OSPF network for topology information:

```
netget> Open OSPF client on 192.168.1.100
LLM: "Send Hello to discover neighbors. Request LSDB for topology map."
```

### 2. OSPF Network Monitoring

Monitor OSPF state changes and neighbor relationships:

```
netget> OSPF client on 10.0.0.1, monitor DR elections
LLM: "Observe Hello packets. Track DR/BDR changes. Alert on priority changes."
```

### 3. Link Cost Analysis

Analyze OSPF link costs and routing decisions:

```
netget> OSPF client, query LSDB and analyze link costs
LLM: "Request LSAs from router 2.2.2.2. Parse link costs. Calculate shortest paths."
```

### 4. OSPF Debugging

Debug OSPF issues without affecting production:

```
netget> OSPF client, passive monitoring
LLM: "Listen for Hello packets. Log neighbor states. Don't respond."
```

## The connected event is acted on (August 2026)

`ospf_client_connected` was raised with an `if let Err(e) = call_llm_for_client(..)` that had
**no success arm at all**. Nothing was named, so nothing read as discarded — it looked like
error handling — and every action the model chose in reply was dropped.

That is the one moment the model is asked what to do *before* any packet arrives, and the
worked example in this very file ("Send Hello, discover neighbours, request LSDB") is exactly
what it would answer with. So the documented flow could never have happened: nothing sent a
Hello unless a packet arrived first, and on a quiet segment none would.

The action-execution loop that already existed inline in `process_ospf_packet` is now
`OspfClient::run_actions`, called by both the connected-event task and the receive loop, so
packet building and `sendto` still exist exactly once. No depth bound is needed: the chain
continues through the receive loop, one packet at a time, as in `datalink`.

**Not proven on the wire.** Everything here needs the raw IP-89 socket, so the only test that
actually multicasts a Hello is `#[ignore]`d behind root — see the injected-commands section
below. The unprivileged tests cover wiring and refusal, not transmission.

## Architecture

### Raw Socket Implementation

Uses IP protocol 89 (same as server):

```rust
let socket = create_ospf_raw_socket(interface_ip, true, false)?;
// - IP protocol 89
// - Joins multicast 224.0.0.5 (AllSPFRouters)
// - Requires root/CAP_NET_RAW
```

### Packet Flow

**Incoming**:

```
OSPF Router → OSPF packet (IP proto 89) → NetGet Client
                                              ↓
                                  Parse IP header, extract OSPF
                                              ↓
                                  Parse OSPF header (type, router_id)
                                              ↓
                                  Create structured JSON event
                                              ↓
                                  Send to LLM for analysis
```

**Outgoing**:

```
LLM → JSON action → execute_action() → ClientActionResult::Custom
                                              ↓
                                  Build OSPF packet from structured data
                                              ↓
                                  send_ospf_packet(socket_fd, dest_ip, bytes)
                                              ↓
                                  Raw sendto() to multicast/unicast
```

### Query Mode Only

**What we DON'T implement**:

- Full OSPF adjacency formation
- Real routing table
- Route installation
- LSA aging/refresh timers
- DR/BDR election participation

**What LLM controls**:

- Send Hello for neighbor discovery
- Request LSDB info (DD packets)
- Request specific LSAs (LSR)
- Parse and analyze received LSAs
- Monitor topology changes

## LLM Integration

### Event Structure (Input to LLM)

When OSPF Hello received:

```json
{
  "event": "ospf_hello_received",
  "data": {
    "neighbor_id": "2.2.2.2",
    "neighbor_ip": "192.168.1.2",
    "area_id": "0.0.0.0",
    "network_mask": "255.255.255.0",
    "hello_interval": 10,
    "router_dead_interval": 40,
    "router_priority": 1,
    "dr": "192.168.1.1",
    "bdr": "0.0.0.0",
    "neighbors": ["1.1.1.1", "3.3.3.3"]
  }
}
```

When Database Description received:

```json
{
  "event": "ospf_database_description_received",
  "data": {
    "neighbor_id": "2.2.2.2",
    "sequence": 12345,
    "init": false,
    "more": true,
    "master": true
  }
}
```

When Link State Update received:

```json
{
  "event": "ospf_link_state_update_received",
  "data": {
    "neighbor_id": "2.2.2.2",
    "lsa_count": 5
  }
}
```

### Action Structure (Output from LLM)

**Send Hello** (neighbor discovery):

```json
{
  "type": "send_hello",
  "router_id": "1.1.1.1",
  "area_id": "0.0.0.0",
  "network_mask": "255.255.255.0",
  "priority": 0,
  "neighbors": [],
  "destination": "multicast"
}
```

**Send Database Description Request**:

```json
{
  "type": "send_database_description",
  "router_id": "1.1.1.1",
  "area_id": "0.0.0.0",
  "sequence": 1,
  "init": true,
  "more": false,
  "master": false,
  "destination": "192.168.1.2"
}
```

**Send Link State Request**:

```json
{
  "type": "send_link_state_request",
  "router_id": "1.1.1.1",
  "area_id": "0.0.0.0",
  "destination": "192.168.1.2"
}
```

**Wait for More Packets**:

```json
{
  "type": "wait_for_more"
}
```

**Disconnect**:

```json
{
  "type": "disconnect"
}
```

## State Management

### Connection State Machine

Client uses state machine to prevent concurrent LLM calls:

- **Idle**: No processing, ready for next packet
- **Processing**: LLM call in progress
- **Accumulating**: LLM busy, queuing packets

**Transitions**:

```
Idle → Processing (receive packet, call LLM)
Processing → Idle (LLM done, no queued packets)
Processing → Accumulating (receive packet while processing)
Accumulating → Processing (LLM done, process queued packet)
```

### Memory Management

LLM can maintain memory across events:

- Track discovered neighbors
- Remember LSDB state
- Track topology changes
- Analyze routing patterns

Memory is optional and controlled by LLM's response.

## Packet Construction

### Hello Packet

Built from LLM JSON action (same as server):

```rust
// Reuses server's build_hello_packet()
let packet = OspfProtocol::build_hello_packet(&action_data)?;
```

### Database Description Packet

```rust
// Reuses server's build_database_description_packet()
let packet = OspfProtocol::build_database_description_packet(&action_data)?;
```

### Link State Request Packet

```rust
// Reuses server's build_link_state_request_packet()
let packet = OspfProtocol::build_link_state_request_packet(&action_data)?;
```

## Sending Packets

### Multicast vs Unicast

```rust
// Send to AllSPFRouters (224.0.0.5)
send_ospf_packet(socket_fd, OSPF_ALL_SPF_ROUTERS, &packet)?;

// Send to specific router
send_ospf_packet(socket_fd, router_ip, &packet)?;
```

### Destination Options

LLM specifies destination in action:

- `"multicast"` → 224.0.0.5 (AllSPFRouters)
- `"dr_multicast"` → 224.0.0.6 (AllDRRouters)
- `"192.168.1.2"` → Unicast to specific router

## Current Implementation Status

### ✅ Completed

- Raw IP socket creation (protocol 89)
- Multicast group join (224.0.0.5)
- IP header parsing
- OSPF header parsing
- Hello packet parsing
- Database Description parsing
- Link State Update parsing
- Structured JSON events to LLM
- Hello packet construction and sending
- DD packet construction and sending
- LSR packet construction and sending
- Multicast and unicast destination support
- Connection state machine
- Memory management

### 📋 TODO

- LSA detailed parsing (Router, Network, Summary LSAs)
- LSA content analysis by LLM
- Topology graph construction
- Scheduled periodic Hellos (if needed)
- Full DD exchange state tracking

## Testing

### With Real OSPF Router (FRR)

**Setup FRR** (on another machine):

```bash
# On Linux router
sudo apt install frr
sudo vi /etc/frr/ospfd.conf

router ospf
  network 192.168.1.0/24 area 0

interface eth0
  ip ospf hello-interval 10
  ip ospf dead-interval 40
```

**Start NetGet Client** (requires root):

```bash
sudo ./netget
netget> open_client ospf 192.168.1.100 "Discover OSPF neighbors and query topology"

# LLM will:
# 1. Receive Hellos from FRR
# 2. Send Hellos back (if instructed)
# 3. Request LSDB via DD packets
# 4. Parse LSAs
```

**Observe FRR**:

```bash
sudo vtysh -c "show ip ospf neighbor"
# May or may not show NetGet (depends on LLM responses)
```

### Current Testing Capabilities

**Can test**:

- Hello packet reception
- Neighbor discovery
- DD packet exchange (partial)
- LSU reception
- Multicast listening
- Protocol parsing

**Cannot test yet**:

- Full adjacency formation
- Complete LSDB synchronization
- LSA detailed analysis
- Topology visualization

## Security Considerations

### Passive Monitoring Mode

LLM can choose to only listen:

- Set priority to 0 (never DR)
- Don't respond to Hellos
- Monitor without participating

### Minimal Impact

Query mode has minimal network impact:

- No route installation
- No packet forwarding
- No DR/BDR participation
- Optional Hello responses

### Use Cases

**Safe for**:

- Production network monitoring
- Topology discovery
- OSPF debugging
- Educational purposes

**Caution**:

- Sending Hellos may create neighbor relationships
- Requesting LSDB creates network traffic
- Use with network admin permission

## Examples

### Passive OSPF Listener

```
netget> open_client ospf 10.0.0.1 "Listen to OSPF packets, don't respond"

LLM receives Hellos, analyzes topology, doesn't send responses
```

### Active Neighbor Discovery

```
netget> open_client ospf 10.0.0.1 "Send Hello, discover neighbors, request LSDB"

LLM sends Hello to multicast, receives responses, requests DB
```

### Topology Mapping

```
netget> open_client ospf 10.0.0.1 "Map entire OSPF network topology in area 0"

LLM:
1. Send Hello → discover neighbors
2. For each neighbor, send DD request
3. Parse LSAs
4. Build topology graph
5. Report: "Found 5 routers, 12 links, 3 networks"
```

## Implementation Notes

### Code Reuse

OSPF client reuses server code:

- `create_ospf_raw_socket()` - Socket creation
- `build_hello_packet()` - Packet construction
- `build_database_description_packet()` - DD construction
- `build_link_state_request_packet()` - LSR construction

### Differences from Server

| Aspect        | Server                 | Client              |
|---------------|------------------------|---------------------|
| **Role**      | Respond to queries     | Query routers       |
| **Neighbors** | Track all neighbors    | Observe neighbors   |
| **LSDB**      | Generate fake LSAs     | Parse real LSAs     |
| **DR/BDR**    | Claim DR (LLM control) | Observe DR          |
| **Adjacency** | Form adjacencies       | Monitor adjacencies |
| **Use Case**  | Honeypot, testing      | Topology discovery  |

### Limitations

**Current**:

- No full adjacency state machine
- No LSA aging/refresh
- No SPF calculation (intentional)
- No route installation (intentional)

**Future Enhancements**:

- Detailed LSA parsing (Router, Network, Summary types)
- Topology graph visualization
- Link cost analysis
- Path calculation (without route installation)

## References

- [RFC 2328 - OSPFv2](https://datatracker.ietf.org/doc/html/rfc2328)
- [OSPF Design Guide (Cisco)](https://www.cisco.com/c/en/us/support/docs/ip/open-shortest-path-first-ospf/7039-1.html)
- [FRR OSPF Documentation](https://docs.frrouting.org/en/latest/ospfd.html)

## Library Choices

**Libraries Used:**

- None (custom implementation)

**Rationale:**

- No mature Rust OSPF client library exists
- Reuses server implementation for packet construction
- Custom raw socket handling via `create_ospf_raw_socket()`
- Direct libc calls for sendto/recvfrom

**Dependencies:**

- `socket2` (via server socket helpers)
- `libc` (raw socket operations)
- Standard tokio async runtime

## LLM Integration Strategy

**Strengths:**

- LLM excellent at analyzing topology data
- JSON events clearly structured
- Protocol parsing handled by code
- LLM focuses on high-level decisions

**Challenges:**

- OSPF protocol complexity (many packet types)
- LSA parsing requires detailed understanding
- Adjacency state machine is complex
- Topology graph reasoning

**Mitigation:**

- Start with Hello-only monitoring (simplest)
- Gradually add DD/LSR support
- LLM receives pre-parsed structured data
- Protocol details hidden from LLM

## Limitations and Known Issues

### Current Limitations

1. **No Full Adjacency Formation**
    - Client doesn't implement full state machine
    - Can send DD/LSR, but doesn't track complete exchange
    - Good for monitoring, not for full OSPF participation

2. **No LSA Content Parsing**
    - LSUs received but LSAs not fully parsed
    - LLM sees "5 LSAs" but not link details
    - TODO: Add Router/Network/Summary LSA parsing

3. **No Periodic Hellos**
    - Client doesn't proactively send Hellos
    - LLM must explicitly request Hello sends
    - TODO: Add scheduled task support

4. **Requires Root**
    - Raw IP sockets need CAP_NET_RAW
    - Cannot run as regular user
    - Platform-specific (Linux/macOS)

### Design Trade-offs

**Simplified State Machine**:

- Pro: Easier to understand and maintain
- Con: Cannot form full adjacencies
- Decision: Query mode doesn't need full adjacency

**No LSDB Storage**:

- Pro: Stateless, LLM maintains context
- Con: Cannot answer queries without re-fetching
- Decision: LLM memory handles context

**Code Reuse from Server**:

- Pro: Consistent packet format, less code
- Con: Tighter coupling with server
- Decision: OSPF packet format is standard

## Future Enhancements

### Priority 1: LSA Parsing

Parse Router/Network/Summary LSA content:

```json
{
  "event": "ospf_lsa_parsed",
  "data": {
    "type": "router",
    "router_id": "2.2.2.2",
    "links": [
      {"type": "transit", "id": "192.168.1.1", "metric": 10},
      {"type": "stub", "id": "10.0.0.0", "mask": "255.255.255.0", "metric": 1}
    ]
  }
}
```

LLM can then:

- Build topology graph
- Calculate link costs
- Identify network segments
- Detect topology changes

### Priority 2: Topology Visualization

Generate topology graph from LSAs:

```
LLM: "Discovered topology:
  Router 1.1.1.1 → Router 2.2.2.2 (cost 10)
  Router 2.2.2.2 → Network 10.0.0.0/24 (cost 1)
  Router 2.2.2.2 → Router 3.3.3.3 (cost 15)
"
```

### Priority 3: Scheduled Hello

Add periodic Hello sending:

```rust
// Use scheduled tasks
let task = ScheduledTask {
    task_id: "hello_periodic".to_string(),
    client_id: Some(client_id),
    recurring: true,
    interval_secs: Some(10),
    instruction: "Send OSPF Hello to multicast".to_string(),
};
```

## Comparison: Full Router vs Query Client

| Feature         | Full OSPF Router   | NetGet Query Client |
|-----------------|--------------------|---------------------|
| Packet RX/TX    | ✅                  | ✅                   |
| Neighbor states | ✅                  | ✅ (partial)         |
| DR/BDR election | ✅ Algorithm        | ❌ Observe only      |
| LSA flooding    | ✅ Automatic        | ❌ Request only      |
| LSDB sync       | ✅ Real sync        | ❌ Query mode        |
| SPF calculation | ✅ Dijkstra         | ❌ None              |
| Routing table   | ✅ Real routes      | ❌ Analysis only     |
| Route install   | ✅ Kernel           | ❌ None              |
| Code complexity | ~10,000 lines      | ~600 lines          |
| Use case        | Production routing | Topology discovery  |

**Winner**: Query client for NetGet's use cases! 🎉

## Injected commands (the dashboard's `[ send ]`)

The client registers a command channel (`command_support::register_command_channel`) as soon as
the raw socket exists, **before** the task that makes the connected-event LLM call. Commands are
drained by their own registered task rather than a `tokio::select!` arm: the receive loop awaits
`call_llm_for_client` inline, so a `select!` arm there would stall for a whole LLM round-trip.
Both tasks use the same raw socket fd.

Injected actions go through `OspfClient::apply_action`, the same function the receive loop uses,
so packet building and `sendto` exist exactly once. `handle_ospf_action` and `send_ospf_packet`
now return the byte count `sendto` accepted rather than `()`, which is what makes a truthful
`Sent` possible at all. Outcomes:

| Injected action | `ClientSendOutcome` |
|---|---|
| `send_hello` / `send_database_description` / `send_link_state_request` / `send_link_state_ack` | `Sent { bytes_sent }` — the count `sendto` returned |
| `wait_for_more` | `Executed { detail: "wait_for_more" }` |
| `disconnect` | `Disconnected` — aborts the receive loop, drops the handle, marks the client Disconnected. OSPF has no wire close |
| unknown action | `Rejected { error }` |

**Privilege.** Everything above needs the raw IP-89 socket, which needs `CAP_NET_RAW`; the
metadata declares `PrivilegeRequirement::RawSockets`, which is what it wants — `Root` would
refuse to start on a capability-only process that could in fact run it. Without the
capability `connect()` fails before the
channel is ever registered, and the contract that failure has to honour is rule 3: **no command
handle is left behind**, so the dashboard greys `[ send ]` out and a late `send_to_client` fails
fast instead of hanging. `tests/client/ospf/command_channel_test.rs` asserts that unprivileged
behaviour on every run and `#[ignore]`s only the test that actually multicasts a Hello.
