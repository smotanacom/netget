# OSPF Protocol Simulator

## Overview

**OSPF protocol simulator** that speaks real OSPF (IP protocol 89) but has **LLM-generated responses** instead of real
routing logic.

**Philosophy**: NetGet handles protocol details, LLM controls behavior

- ✅ Real OSPF protocol (IP 89)
- ✅ Multicast support (224.0.0.5)
- ✅ LLM generates responses **when the operator opts in** (see below)
- ❌ NO real SPF calculation
- ❌ NO real routing table
- ❌ NO actual packet forwarding

### Default behaviour: passive listener, no LLM

Whether to respond at all is **policy**. So with **no operator policy** (no server instruction and
no per-event handler), the server is a **passive listener — no response, no LLM call** per captured
packet (gated by `operator_wants_dynamic` in `mod.rs` = `has_instruction || has_handler`). The model is
consulted **only when the operator opts in** with how the router should behave (an instruction or a
handler). This passive default is compile-verified since the raw-socket path needs root and the
E2E suite never touches it. Everything below describing the LLM generating OSPF responses is the
opt-in path.

**Status**: Experimental (protocol simulator)
**Spec**: [RFC 2328 (OSPFv2)](https://datatracker.ietf.org/doc/html/rfc2328)
**Requires**: `CAP_NET_RAW` (declared as `PrivilegeRequirement::RawSockets`, which is
correct — `Root` would refuse to start on a capability-only process that could in fact
run it)

## Startup parameters

Six declared, six read (`OspfServer::spawn_with_llm_actions` → `OspfInterfaceConfig` in
`actions.rs`):

| Parameter | Default | What reads it |
|---|---|---|
| `router_id` | the interface address | Router ID of every outgoing packet |
| `area_id` | `0.0.0.0` | Area ID of every outgoing packet |
| `network_mask` | `255.255.255.0` | outgoing Hello body; RFC 2328 §10.5 check on inbound Hellos |
| `hello_interval` | `10` | outgoing Hello body; §10.5 check |
| `router_dead_interval` | `40` | outgoing Hello body; §10.5 check |
| `router_priority` | `1` | outgoing Hello body (`priority` field) |

The defaults are RFC 2328 Appendix C.3's, so a server started with no parameters behaves
exactly as it did before the configuration existed.

**Four of the six used to be declared and read nowhere.** `spawn` looked only at
`router_id` and `area_id`; `network_mask`, `hello_interval`, `router_dead_interval` and
`router_priority` were advertised to the model and silently ignored, and the packet
builders fell back to hardcoded constants. Two things now consume them:

1. **Outgoing packets.** `OspfInterfaceConfig::apply_defaults` fills in every field the
   model's action omitted, immediately before `dispatch_event` selects a builder. An
   action that *does* name a field keeps its own value, so a honeypot prompt can still
   deliberately advertise timers that differ from the interface.
2. **Incoming Hellos.** RFC 2328 §10.5 makes matching HelloInterval, RouterDeadInterval
   and network mask a precondition for accepting a Hello at all.
   `OspfInterfaceConfig::hello_mismatches` checks exactly those three. A Hello that fails
   does **not** advance the neighbour state machine — a real router would never reach
   adjacency across such a mismatch, and reporting Init/2-Way would be a lie the operator
   acts on. The event still fires, carrying `config_mismatches` plus
   `local_hello_interval` / `local_router_dead_interval` / `local_network_mask` /
   `local_router_priority`, so the refusal is visible to the model rather than silent.

> **Scope warning.** This is a Hello-level simulator, not a router. It parses every OSPF packet
> type down to its 20-byte LSA headers — never to LSA bodies — and hands the parsed fields to
> the LLM. Outgoing Hello, DD, LSR and LSAck carry real bodies built from the model's
> structured fields; **outgoing LSU always advertises zero LSAs**, because constructing an LSA
> body (Router/Network/Summary link lists) is not implemented and there is no parameter through
> which the model could supply one. So this server can acknowledge LSAs and ask for them by
> name, but can never answer a Link State Request, and adjacency cannot progress past 2-Way.
>
> **No test runs this module at all.** Not "no test against a real router" — no test, full
> stop. `spawn_with_llm_actions` opens a raw IP-89 socket before anything else, so it cannot
> run unprivileged; and the three tests in `tests/server/ospf/e2e_test.rs` named "E2E" start a
> **generic UDP server** (`"base_stack": "UDP"`, which `open_server` renames straight to
> `protocol`) and exchange OSPF-shaped bytes the test itself built. They mock
> `udp_datagram_received` / `send_udp_response`, which belong to `src/server/udp/`; no test in
> the tree mocks an `ospf_*` event or a `send_hello` action. The real coverage here is the
> unit tests in that same file, which call `OspfProtocol` directly.

## Use Cases

### 1. OSPF Honeypot

Detect OSPF reconnaissance and attacks:

```
netget> Listen on interface 192.168.1.100 as OSPF router 10.0.0.1 in area 0
LLM: "Respond to Hellos but log all LSA requests. Advertise fake routes."
```

### 2. Protocol Testing

Test real OSPF routers with controlled responses:

```
netget> OSPF on 10.0.1.1, area 0, be the DR
LLM: "Claim DR priority 255, advertise 3 fake routes, vary LSA ages"
```

### 3. Route Injection

Inject test routes into OSPF networks:

```
netget> OSPF router, inject route 192.168.99.0/24
LLM: "Generate Router LSA with fake link to 192.168.99.0/24, metric 10"
```

### 4. OSPF Education

Learn OSPF without setting up real routers:

```
netget> Be an OSPF router, explain what you're doing
LLM: "Received Hello from 2.2.2.2. Sending Hello back with priority 1..."
```

## Architecture

### Raw Socket Implementation

Uses IP protocol 89 (not UDP):

```rust
let socket = create_ospf_raw_socket(interface_ip, true, false)?;
// - IP protocol 89
// - Joins multicast 224.0.0.5 (AllSPFRouters)
// - Requires root/CAP_NET_RAW
```

### Packet Flow

**Incoming**:

```
Real Router → OSPF packet (IP proto 89) → NetGet
                                           ↓
                               Parse IP header, extract OSPF
                                           ↓
                               Parse OSPF header (ver, type, router_id)
                                           ↓
                               Create structured JSON event
                                           ↓
                   Opt-in only: send to handler/LLM with context
                   Default (no policy): observe passively, no LLM call
```

**Outgoing**:

```
LLM → JSON action → execute_action() → ActionResult::Custom{ name: "ospf_action", data }
                                           ↓
                               dispatch_event() in mod.rs matches on data["type"]
                                           ↓
                               OspfProtocol::build_*_packet(data)  ← structured JSON, no bytes
                                           ↓
                               send_ospf_packet(socket_fd, dest_ip, bytes)
```

**Architecture**: actions return `ActionResult::Custom` carrying the structured action JSON - never
packet bytes, which the LLM could not produce reliably. `dispatch_event()` selects the builder,
resolves `destination` (`"multicast"` → 224.0.0.5, `"dr_multicast"` → 224.0.0.6, or a unicast IP)
and writes to the raw socket FD held in `OspfState`.

### No Real Routing

**What we DON'T implement**:

- SPF (Dijkstra) calculation
- Real routing table
- Route installation (netlink)
- DR/BDR election algorithm
- LSA aging timers
- Proper LSDB synchronization

**What LLM controls** (opt-in mode only; the default is a passive listener that never calls the LLM):

- Whether to respond to Hellos
- What routes to advertise (fake or real)
- DR/BDR claim (just set priority)
- LSA content (manually crafted)
- Neighbor acceptance/rejection

## LLM Integration

### Event Structure (Input to LLM)

When OSPF Hello received:

```json
{
  "event": "ospf_hello",
  "data": {
    "connection_id": "conn-12345",
    "neighbor_id": "2.2.2.2",
    "neighbor_ip": "192.168.1.2",
    "area_id": "0.0.0.0",
    "network_mask": "255.255.255.0",
    "hello_interval": 10,
    "router_dead_interval": 40,
    "router_priority": 1,
    "dr": "0.0.0.0",
    "bdr": "0.0.0.0",
    "neighbors": ["1.1.1.1", "3.3.3.3"],
    "local_network_mask": "255.255.255.0",
    "local_hello_interval": 10,
    "local_router_dead_interval": 40,
    "local_router_priority": 1,
    "config_mismatches": []
  }
}
```

`local_*` are this interface's configured values and `config_mismatches` lists the RFC
2328 §10.5 fields that differ. A non-empty list means no adjacency can form with that
neighbour until one side is reconfigured.

### Action Structure (Output from LLM)

**Send Hello Response**:

```json
{
  "type": "send_hello",
  "router_id": "1.1.1.1",
  "area_id": "0.0.0.0",
  "network_mask": "255.255.255.0",
  "priority": 100,
  "dr": "1.1.1.1",
  "bdr": "0.0.0.0",
  "neighbors": ["2.2.2.2"],
  "destination": "multicast"
}
```

**Send Database Description (unicast to specific neighbor)**:

```json
{
  "type": "send_database_description",
  "router_id": "1.1.1.1",
  "area_id": "0.0.0.0",
  "sequence": 12345,
  "init": false,
  "more": true,
  "master": true,
  "destination": "192.168.1.2"
}
```

**Generate Fake Router LSA** (TODO):

```json
{
  "type": "send_lsa",
  "lsa_type": "router",
  "router_id": "1.1.1.1",
  "links": [
    {"type": "stub", "id": "10.0.0.0", "mask": "255.255.255.0", "metric": 10},
    {"type": "stub", "id": "10.0.1.0", "mask": "255.255.255.0", "metric": 20}
  ],
  "destination": "multicast"
}
```

## State Management

### Neighbor Tracking

```rust
struct OspfNeighbor {
    router_id: String,
    neighbor_ip: Ipv4Addr,
    state: OspfNeighborState,  // Down/Init/2-Way/...
    priority: u8,
    dr: String,
    bdr: String,
    last_hello: Instant,
}
```

**State Transitions** (simplified):

- Down → Init (receive first Hello)
- Init → 2-Way (bidirectional Hello)
- 2-Way → ExStart (form adjacency - TODO)
- ExStart → Exchange (DD packets - TODO)
- Exchange → Loading (LSR/LSU - TODO)
- Loading → Full (synchronized - TODO)

**Currently**: Only Down → Init → 2-Way implemented

### No LSDB

No real Link State Database. LLM generates LSAs on demand:

- LLM remembers what it advertised (via conversation history)
- LLM generates fake LSAs when requested
- No LSA aging, no MaxAge, no refresh timers

## Packet Construction

### Hello Packet

Built from LLM JSON action:

Every field below reads "from LLM", but the action rarely names them all: whatever it
omits comes from the interface configuration (`apply_defaults`), not from a hardcoded
constant.

```rust
// OSPF Header (24 bytes)
- Version: 2
- Type: 1 (Hello)
- Packet Length: calculated
- Router ID: from LLM
- Area ID: from LLM
- Checksum: standard IP one's-complement checksum (see below)
- Auth Type: 0 (none)
- Authentication: zeros

// Hello Body
- Network Mask: from LLM
- Hello Interval: from LLM (default 10s)
- Options: 0
- Router Priority: from LLM
- Router Dead Interval: from LLM (default 40s)
- Designated Router: from LLM
- Backup DR: from LLM
- Neighbors: list from LLM
```

### Checksum (RFC 2328 A.3.1)

The OSPF **packet** checksum is the standard IP one's-complement checksum over the whole packet
with the checksum field zeroed and the 64-bit authentication field (header bytes 16..24)
excluded. It is **not** the Fletcher checksum of Section D.4 - Fletcher applies to LSA headers.

This module used Fletcher until it was corrected. The consequence was total: every packet NetGet
emitted failed the receiver's validity check, so FRR/BIRD dropped all of them silently. The
defining property, and the check a receiver performs, is that recomputing the sum over the packet
with the checksum left in place yields 0. `OspfProtocol::calculate_checksum` satisfies it.

**The direction matters, and getting it wrong is invisible.** `calculate_checksum` sums the
packet exactly as given and does **not** special-case bytes 12..14, because that single
behaviour is what lets one function serve both ends of the standard one's-complement idiom:

| | what the caller does |
|---|---|
| sending | leave the checksum field zero, call it, store the result there |
| receiving | call it over the packet as it arrived; a valid packet yields 0 |

A version that zeroed bytes 12..14 unconditionally collapsed those into one. The receiver then
recomputed the *sender's* value instead of 0, so the validity check could never pass and a
corrupted packet was indistinguishable from a good one — while the doc comment asserted the
yields-0 property the code had just made unsatisfiable. Each `build_*_packet` now zeroes the
field immediately before the call, so the sending contract is local rather than an assumption
about header code thirty lines earlier.

Note that a test asserting only "a corrupted packet does **not** validate" passes under the
broken version too, because it never returned 0 for anything. The yields-0 assertion is the one
that carries the weight; keep both.

### LSA headers: carried. LSA bodies: not implemented.

The distinction matters, because three of the four packet types only ever needed headers.

**Carried.** An LSA *header* is the fixed 20 bytes of RFC 2328 A.4.1 — age, options, type,
Link State ID, Advertising Router, sequence, checksum, length. `OspfProtocol::parse_lsa_headers`
reads them and every DD/LSU/LSAck event reports them as structured JSON;
`send_link_state_ack` and `send_database_description` take an `lsa_headers` array in that same
shape and serialise it back, and `send_link_state_request` takes a `requests` array of
LSType/LinkStateID/AdvertisingRouter triples (A.3.4). So the model answers a flood by handing
the event's `lsa_headers` straight back, and asks for LSAs by naming them.

This is not cosmetic. RFC 2328 §13.7 matches an acknowledgement against the neighbour's
retransmission list **header by header, checksum included** — so the body-less LSAck this used
to emit acknowledged nothing, and the neighbour retransmitted every LSA each RxmtInterval
until the adjacency failed. An empty LSAck is a positive assertion that is simply false, not a
harmless no-op.

**Not implemented.** LSA *bodies* — Router LSA link lists, Network LSA attached-router lists,
Summary LSA network+metric. `send_link_state_update` therefore always writes an LSA count of
zero and there is no parameter through which the model could supply contents. A Link State
Request from a peer consequently cannot be satisfied, which is why the `ospf_link_state_request`
event tells the model to answer `wait_for_more` rather than pretending. This is the single
remaining gap that keeps adjacency at 2-Way.

## Sending Packets

### Multicast vs Unicast

```rust
// Send to AllSPFRouters (224.0.0.5)
send_ospf_packet(socket_fd, Ipv4Addr::new(224, 0, 0, 5), packet_bytes)?;

// Send to specific neighbor
send_ospf_packet(socket_fd, neighbor_ip, packet_bytes)?;
```

### Raw Socket Sendto

```rust
unsafe {
    let dest_addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: 0,  // Raw IP, no port
        sin_addr: libc::in_addr {
            s_addr: u32::from(dest_ip).to_be(),
        },
        sin_zero: [0; 8],
    };

    libc::sendto(
        socket_fd,
        packet.as_ptr() as *const libc::c_void,
        packet.len(),
        0,
        &dest_addr as *const _ as *const libc::sockaddr,
        std::mem::size_of::<libc::sockaddr_in>() as u32,
    );
}
```

## Current Implementation Status

### ✅ Completed

- Raw IP socket creation (protocol 89)
- Multicast group join (224.0.0.5)
- IP header parsing
- OSPF header parsing
- Hello packet parsing (full)
- DD / LSR / LSU / LSAck parsing to their headers, each raising its own LLM event with parsed
  fields (DD flags + sequence + LSA headers; LSR request triples; LSU LSA headers walked by each
  LSA's own length field; LSAck LSA headers)
- Neighbor state tracking (Down/Init/2-Way)
- Dead neighbour ageing: a neighbour silent for the configured `router_dead_interval` is
  dropped on the next received packet. This is also what bounds the neighbour table — the key
  is the sender's Router ID, four bytes it chooses, so before this a peer spraying Hellos with
  random Router IDs grew the map without limit
- Neighbours published to `AppState` via `add_connection_to_server` + `update_connection_stats`,
  so they appear as peers in the dashboard rail with live ↓/↑ counters, and `call_llm` can
  resolve the peer address for the event and access logs. The `ConnectionId` used to be minted
  and written into every event without ever being registered, so every OSPF log line recorded a
  null client address. `metadata()` declares `.connectionless()`, so the 10s idle sweep reaps
  these entries — OSPF has no close to key removal on
- Structured JSON events to LLM - all five event types declare their actions, so the model is
  actually offered tools (`EventType::actions` is what `call_llm` advertises, not
  `get_sync_actions()`)
- Hello packet construction
- Packet transmission function
- Connect LLM actions to packet sending
    - LLM JSON responses converted to OSPF packets
    - Packets sent to multicast (224.0.0.5) by default
    - Full logging (dual tracing + status_tx)
- Unicast destination support
    - LLM can specify "multicast" (224.0.0.5), "dr_multicast" (224.0.0.6), or unicast IP (e.g., "192.168.1.2")
    - Useful for targeted Database Description/LSU exchanges with specific neighbors

### 📋 NOT IMPLEMENTED

- **LSA *body* construction** (Router, Network, Summary link lists) - the single biggest
  remaining gap; without it `send_link_state_update` can only advertise zero LSAs, a peer's
  Link State Request cannot be answered, and adjacency cannot leave 2-Way. LSA *headers* are
  carried on DD/LSR/LSAck - see the section above for why that is a different thing
- DD exchange *state* (master/slave negotiation, sequence tracking) - packets are parsed and
  surfaced, and the model can echo LSA headers, but nothing tracks the exchange
- Periodic Hello timer - the server only ever replies, it never initiates
- DR/BDR election (the LLM claims a role by setting priority; no algorithm runs)
- LSDB, SPF, routing table, route installation - out of scope by design

## Testing

### With Real OSPF Router (FRR)

**Setup FRR**:

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

**Start NetGet** (requires root):

```bash
sudo ./netget
netget> Listen on interface 192.168.1.100 as OSPF router 192.168.1.100 in area 0

# FRR will send Hellos to 224.0.0.5
# NetGet receives, parses, sends event to LLM
# LLM decides response
# NetGet sends Hello back
```

**Observe FRR**:

```bash
sudo vtysh -c "show ip ospf neighbor"
# Should see NetGet in Init/2-Way if the LLM responds correctly.
# It will never reach Full: that needs DD exchange with real LSA headers.
```

**This has not been done.** Until the checksum was corrected, FRR would have discarded every
packet before parsing it. Nobody has since re-run the experiment, and raw sockets need root.

### Current Limitations

**Cannot test yet**:

- Full adjacency formation (need LSA construction, not just DD/LSR/LSU parsing)
- Route advertisement (need LSA generation)
- Route redistribution
- SPF calculation (we don't do this)
- Multi-area OSPF

**Can test**:

- Hello packet exchange
- Neighbor discovery
- State transitions (Down/Init/2-Way)
- Multicast reception
- Protocol parsing

**What `tests/server/ospf/e2e_test.rs` actually asserts.** Its three "E2E" tests are not
OSPF tests: they start a generic UDP server and relay OSPF-shaped bytes the test itself
built (see the scope warning at the top). Nothing anywhere drives this module's I/O.

The rest of the file is real, and tests `OspfProtocol` directly:

- **Checksum** — that recomputing `calculate_checksum` over a built packet yields 0, which
  is precisely the validity check a receiver runs; that a corrupted body fails it; and that
  the 64-bit authentication field is excluded, so filling it in does not invalidate the
  packet. Until this pass the only checksum test in the file exercised the *test's own*
  Fletcher helper — the algorithm this module records as the bug that made FRR/BIRD drop
  every packet — and asserted only that it was non-zero.
- **Startup config → wire**, at RFC 2328 A.3.2 offsets: a configured `hello_interval` /
  `router_dead_interval` / `router_priority` / `network_mask` reaching bytes 28-29, 32-35,
  31 and 24-27 of the Hello body; an action-supplied value overriding the configured one;
  non-Hello packets taking only `router_id`/`area_id`.
- **LSA headers and request triples** at A.4.1/A.3.4 offsets, including that the LS checksum
  is echoed verbatim, that a multi-header LSAck still validates, that both bodies remain
  optional, and that what a peer would parse back equals the header the model supplied.
- **A malformed dotted quad is rejected** rather than silently becoming 0.0.0.0.
- The §10.5 mismatch check firing on exactly the three fields the RFC names, and the
  declared startup-parameter set equalling the set the implementation reads.

## Security Considerations

### Honeypot Mode

Detect OSPF attacks:

- Neighbor scanning
- Route poisoning attempts
- LSA flooding
- Area spoofing

LLM can log malicious behavior and respond defensively.

### Route Injection Risks

**Be careful**: Advertising fake routes in production networks can:

- Cause routing loops
- Black-hole traffic
- Break connectivity
- Trigger security alerts

**Use only**:

- Test networks
- Isolated environments
- With network admin permission

## Examples

### Passive OSPF Listener

```
netget> OSPF on 10.0.0.1, area 0, priority 0, log all packets

LLM receives Hellos, logs them, doesn't respond (priority 0 = never DR)
```

### Aggressive DR Claim

```
netget> OSPF router 10.0.0.1, area 0, become DR immediately

LLM responds with:
{
  "type": "send_hello",
  "priority": 255,
  "dr": "10.0.0.1",
  "bdr": "0.0.0.0"
}
```

### Fake Route Advertisement (NOT IMPLEMENTED - no action accepts LSA contents)

```
netget> OSPF router, advertise fake routes to 192.168.99.0/24

LLM generates:
{
  "type": "send_lsa",
  "lsa_type": "router",
  "links": [{"id": "192.168.99.0", "mask": "255.255.255.0", "metric": 1}]
}
```

## Missing Features & Implementation Guide

### 1. LSA Packet Construction (Router, Network, Summary)

**What's Missing**: Actions can send empty LSU packets, but can't construct LSA contents.

**How to Implement**:

1. Add LSA parameters to `send_link_state_update` action:
   ```json
   {
     "type": "send_link_state_update",
     "router_id": "1.1.1.1",
     "lsas": [
       {
         "type": "router",  // Router LSA (Type 1)
         "age": 0,
         "sequence": 0x80000001,
         "links": [
           {"type": "stub", "id": "10.0.0.0", "data": "255.255.255.0", "metric": 10},
           {"type": "transit", "id": "192.168.1.1", "data": "192.168.1.2", "metric": 1}
         ]
       }
     ],
     "destination": "multicast"
   }
   ```

2. Update `execute_send_link_state_update()` in actions.rs:
    - Parse `lsas` array from JSON
    - For each LSA, serialize to OSPF LSA format:
        - LSA header (20 bytes): age, options, type, link state ID, advertising router, sequence, checksum, length
        - LSA body varies by type
    - Router LSA (Type 1): links array with type, ID, data, metric
    - Network LSA (Type 2): network mask + attached routers
    - Summary LSA (Type 3): network + mask + metric

3. LSA checksum: Use Fletcher checksum over LSA header+body (same as packet checksum but excludes age field)

**Effort**: Medium (2-3 hours). Main challenge is LSA format serialization.

**Priority**: Medium. Needed for full OSPF database exchange, but simulator works without it.

### 2. Database Description Handling (DD Exchange)

**What's Missing**: the exchange *state*. DD packets are parsed and their LSA headers reach the
model, and `send_database_description` takes an `lsa_headers` array back, but nothing tracks
master/slave or sequence continuity across packets — the model has to hold that itself.

**How to Implement**:

1. Add DD event parsing in mod.rs:
    - Extract DD flags (I, M, MS), sequence number, MTU
    - Parse LSA headers from DD body (20 bytes each)
    - Send structured JSON to LLM with LSA summaries

2. Add connection state tracking:
    - Current: `OspfNeighborState` is an enum (Down/Init/2-Way/...)
    - Add: `dd_sequence: u32`, `dd_master: bool`, `lsa_requests: Vec<LsaHeader>`

3. LLM prompt additions:
    - "You received Database Description with LSA headers: ..."
    - "You are master/slave in DD exchange"
    - "Respond with your LSA headers or send LSR for missing LSAs"

**Effort**: Low (1-2 hours). Mostly parsing + state tracking.

**Priority**: High. Required for neighbor adjacency formation.

### 3. LSR/LSU/LSAck Handling

**Mostly done.** Incoming LSR/LSU/LSAck are parsed and raise their own events with structured
fields, and outgoing LSR and LSAck now carry real bodies (request triples / LSA headers). What
remains is `send_link_state_update`, which cannot carry LSAs because LSA *bodies* are not
implemented — see item 1 — and any notion of retained state.

**How to Implement**:

1. Parse incoming LSR in mod.rs:
    - Extract requested LSA identifiers (type, ID, advertising router)
    - Send JSON event to LLM: `{"event": "ospf_lsr", "requests": [...]}`

2. LLM decides:
    - If we "have" the LSA → `send_link_state_update` with LSA content
    - If we don't → ignore or log

3. Parse incoming LSU:
    - Extract LSAs from packet body
    - Send to LLM: `{"event": "ospf_lsu", "lsas": [...]}`
    - LLM can log, respond with LSAck, or ignore

4. Parse incoming LSAck:
    - Extract acknowledged LSA headers
    - Send to LLM for logging

**Effort**: Medium (2-3 hours). Parsing is straightforward, but LLM guidance needs refinement.

**Priority**: Medium. Nice for completeness, but not critical for basic simulator.

### 4. Periodic Hello Timer

**What's Missing**: Server only responds to incoming packets. Doesn't proactively send Hellos.

**How to Implement**:

1. Use NetGet's scheduled tasks system (see CLAUDE.md § Scheduled Tasks):
   ```rust
   // In spawn_with_llm_actions, add server-scoped task
   let hello_task = ScheduledTask {
       task_id: "hello_broadcast".to_string(),
       server_id: Some(server_id),
       connection_id: None,  // Server-scoped
       recurring: true,
       interval_secs: Some(10),  // Every 10s
       instruction: "Send OSPF Hello to multicast (224.0.0.5) with current DR/BDR state".to_string(),
   };
   ```

2. LLM receives periodic prompt, responds with `send_hello` action

3. Alternative: Implement in mod.rs with tokio::interval:
   ```rust
   let mut hello_timer = tokio::time::interval(Duration::from_secs(10));
   loop {
       tokio::select! {
           _ = hello_timer.tick() => {
               // Send multicast Hello
           }
           // ... packet receive logic
       }
   }
   ```

**Effort**: Low (1 hour with scheduled tasks, 2 hours with tokio::select).

**Priority**: High. Required for maintaining neighbor relationships (40s dead timer).

### 5. Dead Neighbor Detection — DONE

Neighbours silent for the configured `router_dead_interval` are dropped, in
`handle_ospf_packet`, under the lock it already takes. It is *lazy* — the sweep runs when the
next packet arrives, not on a timer — which is enough for both jobs it does: RFC 2328 §10.5
correctness, and bounding the table against a peer spraying Hellos with attacker-chosen Router
IDs. A timer-driven version would additionally let a neighbour go Down while the interface is
completely silent; that needs the periodic Hello timer (item 4) to be worth having.

Note the ordering constraint the implementation has to respect and the obvious version does
not: allocating the `ConnectionId` calls `get_next_unified_id`, which takes the global
`AppState` write lock. Doing that while holding the neighbour mutex is a lock held across an
`.await` — forbidden here, and a lock-order inversion against every task that takes `AppState`
first. So the lookup and the allocation are separate critical sections, and the insert
re-checks for a racing packet from the same neighbour.

### 6. DR/BDR Election Logic

**What's Missing**: LLM manually claims DR/BDR via Hello priority. No automatic election.

**How to Implement**:

1. After receiving Hello from all neighbors, run election:
    - Highest priority router with no DR → becomes DR
    - Second-highest → becomes BDR
    - In tie, highest router ID wins

2. Update Hello responses:
    - If we won election → send Hello with dr="our_ip"
    - If we lost → send Hello with dr="winner_ip"

3. Implementation options:
    - **Pure LLM**: Send neighbor list + priorities to LLM, let it decide
    - **Hybrid**: Run election in Rust, send result to LLM for approval
    - **Manual**: Keep current behavior (LLM decides in prompts)

**Effort**: Medium (2-3 hours for full election, 30min for hybrid).

**Priority**: Low. Manual DR claiming works for simulator use case.

## References

- [RFC 2328 - OSPFv2](https://datatracker.ietf.org/doc/html/rfc2328)
- [OSPF Design Guide (Cisco)](https://www.cisco.com/c/en/us/support/docs/ip/open-shortest-path-first-ospf/7039-1.html)
- [FRR OSPF Documentation](https://docs.frrouting.org/en/latest/ospfd.html)

## Comparison: Full Router vs Simulator

| Feature         | Full OSPF Router   | NetGet Simulator |
|-----------------|--------------------|------------------|
| Packet RX/TX    | ✅                  | ✅                |
| Neighbor states | ✅                  | ✅ (partial)      |
| DR/BDR election | ✅ Algorithm        | ❌ LLM claims     |
| LSA flooding    | ✅ Automatic        | ❌ LLM manual     |
| LSDB sync       | ✅ Real sync        | ❌ No LSDB        |
| SPF calculation | ✅ Dijkstra         | ❌ None           |
| Routing table   | ✅ Real routes      | ❌ Fake routes    |
| Route install   | ✅ Kernel           | ❌ None           |
| Code complexity | ~10,000 lines      | ~500 lines       |
| Use case        | Production routing | Testing/honeypot |

**Winner**: Simulator for NetGet's use cases! 🎉
