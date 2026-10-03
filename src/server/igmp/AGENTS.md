# IGMP Protocol Implementation

## Overview

IGMP (Internet Group Management Protocol) is used by IPv4 hosts and adjacent routers to establish multicast group
memberships. Handling is **not wire-determined** — which groups to report is membership policy, and an observed
report/leave needs no reply.

### Default behaviour: static, no LLM

With **no operator policy** (no server instruction and no per-event handler), the server applies the spec-safe
**static default: advertise no memberships and stay silent**, with **no LLM round-trip** per captured message
(gated by `operator_wants_dynamic` in `mod.rs` = `has_instruction || has_handler`). The model is consulted **only when the
operator opts in** by supplying the membership policy it should apply (an instruction or a handler). The
`LLM Decision Making` material below therefore describes the opt-in path, not the default.

## Protocol Details

**Standard**: RFC 2236 (IGMPv2), RFC 3376 (IGMPv3)
**Transport**: IP Protocol 2 (raw IP packets)
**Port**: N/A (operates at IP layer)
**Versions Supported**: IGMPv1, IGMPv2 (partial IGMPv3)

### Message Types

1. **Membership Query (0x11)**: Sent by routers to discover which groups have members
    - General Query: group_address = 0.0.0.0
    - Group-Specific Query: group_address = specific multicast group

2. **Membership Report (0x16)**: Sent by hosts to join a group or respond to queries
    - IGMPv1: type 0x12
    - IGMPv2: type 0x16
    - IGMPv3: type 0x22

3. **Leave Group (0x17)**: Sent by hosts to leave a group (IGMPv2+)

### IGMP Packet Format (8 bytes minimum)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     Type      | Max Resp Time |           Checksum            |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         Group Address                         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

## Library Choices

### Server Library

**Implementation**: Raw IP sockets using `libc` and `socket2::Socket`

**Socket Creation**:

- Domain: `AF_INET` (IPv4, constant 2)
- Type: `SOCK_RAW` (constant 3)
- Protocol: `IPPROTO_IGMP` (constant 2)

**Requirements**:

- Root privileges (Linux also accepts `CAP_NET_RAW`)
- Any Unix: `AF_INET`, `SOCK_RAW` and `IPPROTO_IGMP` are POSIX constants, not Linux-specific, so
  the same code path runs on Linux, macOS and the BSDs. Windows is excluded from `dist-windows`
  because there is no raw-socket path there.
- Multicast-enabled network interface

**Implementation Details**:

- Raw socket receives full IP packets (including IP header)
- IP header is stripped before IGMP parsing by `igmp_payload()`, a pure function so it is
  testable without a socket. The IHL field is four bits, so a hostile packet can name a header
  of 0..=60 bytes: `igmp_payload` rejects anything outside 20..=60 or longer than the packet.
  Without the lower bound the IP header itself would be parsed as IGMP; without the upper one
  the slice would be out of range and panic inside the capture task, where `tokio::spawn`
  swallows it and the server goes on reporting `Running`.
- The RFC 1071 checksum **is verified** (`IgmpMessage::checksum_valid`, folding the whole
  message through the same `igmp_checksum` the builders use and requiring 0). A message that
  fails it is logged `decision=bad_checksum` at WARN and dropped: acting on it would spend an
  LLM round-trip, and possibly a Membership Report, on bytes no conforming receiver would have
  accepted. Parsing and verification are deliberately separate so "not IGMP" stays
  distinguishable from "IGMP, arrived corrupt".
- Multicast group join/leave uses `join_multicast_v4`/`leave_multicast_v4`
- IGMP packets sent to appropriate multicast addresses, by `response_destination()` (pure,
  and covered field by field in `tests/server/igmp/packet_codec_test.rs`):
    - v1/v2 Membership Report (0x12/0x16) → the group address itself (RFC 2236 §9)
    - Leave Group (0x17) → ALL-ROUTERS 224.0.0.2 (RFC 2236 §2.9)
    - IGMPv3 Membership Report (0x22) → 224.0.0.22 (RFC 3376 §4.2.14)
    - anything else, or a "report" naming a non-multicast group → back to the sender
  The v1 and v3 cases used to fall into the sender fallback, which puts a multicast control
  message on a unicast address where no router is listening for it.

### Client Library (for testing)

**Option 1**: Manual packet construction with `socket2`
**Option 2**: Use system multicast join/leave (automatic IGMP)

## LLM Integration

### Control Points

**Async Actions** (require external state changes):

- `join_group` - Join a multicast group
- `leave_group` - Leave a multicast group

**Sync Actions** (immediate packet responses):

- `send_membership_report` - Send IGMP Membership Report
- `send_leave_group` - Send IGMP Leave Group message
- `ignore_message` - Don't respond to this message

Every one of these names a `group_address`, and all four verbs **require it to be inside
224.0.0.0/4**. That is not defensive tidiness: a General Query names group `0.0.0.0`, so the
obvious-looking answer — echo the queried group back into a report — reports membership in a
group that does not exist. Which groups to report is membership policy and is never derivable
from a general query. `join_multicast_v4` would also refuse a unicast address with a bare
`EINVAL`, which tells the model nothing about which field was wrong.

**What the model is offered, per event.** `call_llm` builds the tool list from
`event.event_type.actions`, not from `get_sync_actions()`, so a query offers
`send_membership_report` + `ignore_message` and an observed report/leave offers `ignore_message`
alone. `send_leave_group` is deliberately on no event — a leave is unsolicited, not an answer to
something received — and stays reachable through the dashboard composer and static routing
rules, which do read `get_sync_actions()`.

### Events

1. **igmp_query_received**: Router querying for group members
    - Parameters: query_type, group_address, max_response_time
    - Common response: send_membership_report (if member of queried group)

2. **igmp_report_received**: Another host reporting group membership
    - Parameters: group_address
    - Common response: ignore_message (suppress own report per IGMP)

3. **igmp_leave_received**: Another host leaving a group
    - Parameters: group_address
    - Common response: ignore_message

### LLM Decision Making

The LLM controls:

1. **Group Membership**: Which multicast groups to join/leave
2. **Query Responses**: Whether to respond to membership queries
3. **Report Suppression**: IGMPv2 includes report suppression - if we hear another host's report, we can cancel our own
4. **Timing**: When to send unsolicited reports

## Logging Strategy

Follows NetGet dual logging pattern (tracing + status_tx):

- **ERROR**: Failed to parse IGMP packet, socket errors
- **WARN**: Raw socket limitations, privilege issues
- **INFO**: Group join/leave events, LLM messages
- **DEBUG**: Message summaries (type, group, source)
- **TRACE**: Full packet hex dumps

Examples:

```rust
debug!("IGMP received from {}: {}", peer_addr, igmp_msg.description());
let _ = status_tx.send(format!("[DEBUG] IGMP received from {}: {}", peer_addr, igmp_msg.description()));

trace!("IGMP data (hex): {}", hex_str);
let _ = status_tx.send(format!("[TRACE] IGMP data (hex): {}", hex_str));
```

## Architecture Details

### Connection Tracking

IGMP is connectionless (like UDP). We track:

- Recent peers that sent IGMP messages
- Joined multicast groups
- Last activity timestamp

### State Machine

Server maintains `IgmpServerState`:

- `joined_groups`: the groups actually joined on the raw socket. It is read, not merely written:
  a repeated `join_group` for a group already in the set is skipped, because `join_multicast_v4`
  would otherwise fail with `EADDRINUSE` and be logged as an error when the membership the model
  asked for already exists.

### Packet Processing Flow

1. Receive raw IP packet from socket (includes IP header)
2. Strip IP header using IHL (Internet Header Length) field
3. Verify protocol is IGMP (IP protocol 2)
4. Parse IGMP message (type, max_response_time, group_address, checksum), then verify the
   checksum and drop the message if it fails
5. Create connection state with protocol info
6. Determine event type based on message type
7. **Default (no operator policy)** → apply the static default (advertise nothing, stay silent), **no LLM call**.
   **Opt-in only** (instruction or handler) → call the handler/LLM with the event
8. Execute actions (opt-in path only):
    - Sync: Build and send IGMP response packet to multicast address
    - Async: Perform actual multicast join/leave via socket options

## Implementation Details

### Current Implementation

**Raw Socket Support**: ✅ Fully implemented

- Uses `libc::socket()` with SOCK_RAW and IPPROTO_IGMP, then takes ownership via
  `Socket::from_raw_fd`. The fd is validated (`>= 0`) **before** the wrap - wrapping first would
  break `from_raw_fd`'s safety contract and make `Drop` call `close(-1)`.
- Requires root privileges or CAP_NET_RAW capability. Declared as
  `PrivilegeRequirement::RawSockets` in the protocol metadata.
- Socket creation and setup failures propagate out of `Server::spawn()` (dual-logged), so the
  server is recorded as `ServerStatus::Error(..)` and never as `Running`. An unprivileged MCP
  caller sees:

  ```
  Failed to start server: Failed to create raw IGMP socket on 127.0.0.1: Operation not permitted
  (os error 1) (IGMP needs root/CAP_NET_RAW - re-run netget with sudo)
  ```
- Receives and parses real IGMP packets from network

**Multicast Group Management**: ✅ Fully implemented

- Uses `join_multicast_v4()` to actually join multicast groups
- Uses `leave_multicast_v4()` to actually leave groups
- Kernel handles IP-level multicast membership

**IP Header Handling**: ✅ Implemented

- Extracts IHL field to determine IP header length
- Strips IP header before IGMP parsing
- Validates IP protocol field (must be 2 for IGMP)

### Limitations

1. **IGMPv3 Support**: Partial
    - Can parse IGMPv3 reports (type 0x22)
    - Cannot construct IGMPv3 reports with source lists
    - No source filtering (INCLUDE/EXCLUDE modes)

2. **Router Functionality**: Not implemented
    - Current implementation is host-side only
    - Router would need to send queries and track group members

3. **Platform**: Unix (Linux / macOS / BSD)
    - Uses only portable POSIX constants (`AF_INET` / `SOCK_RAW` / `IPPROTO_IGMP`); the earlier
      "Linux-only" claim in this document was wrong
    - Exercised mainly on Linux; macOS compiles and startup is verified, but multicast behaviour
      there is less covered
    - Windows: not built (`igmp` is excluded from the `dist-windows` feature set)

4. **No interface selection**: IGMP is the one raw-socket protocol here with no `default_binding()`,
   so it takes the legacy port-based startup path and a `port` argument is **required** even though
   raw sockets ignore ports. An `interface` passed via MCP is silently discarded; the socket binds
   to the `host` address instead (0.0.0.0 when unset).

### Future Enhancements

1. **IGMPv3 Source Filtering**:
    - INCLUDE mode: specific sources
    - EXCLUDE mode: all except specific sources

3. **Router Mode**:
    - Send periodic general queries
    - Track per-interface group membership
    - Handle leave group messages with group-specific queries

4. **Automatic Report Suppression**:
    - Random delay before sending reports
    - Cancel report if another host reports first

## Example Prompts

### Basic Multicast Group Member

```
Create an IGMP server that joins multicast group 239.255.255.250 and
responds to membership queries with reports for that group.
```

Expected behavior:

1. Server starts
2. LLM decides to join 239.255.255.250
3. On query for 0.0.0.0 (general), sends report for 239.255.255.250
4. On query for 239.255.255.250, sends report
5. On query for other groups, ignores

### SSDP/UPnP Device

```
Create an IGMP server for a UPnP device. Join the SSDP multicast group
239.255.255.250 and respond to all membership queries.
```

Expected behavior:

1. Join SSDP multicast group (239.255.255.250)
2. Respond to all general queries
3. Respond to group-specific queries for SSDP group

### Report Suppression Example

```
Create an IGMP server that implements report suppression. Join group
224.0.1.1, but if you receive another host's report for that group
within the max response time window, don't send your own report.
```

Expected behavior:

1. Join group 224.0.1.1
2. On general query, set timer to send report
3. If receive another host's report for 224.0.1.1, suppress own report
4. If timer expires without seeing report, send report

## Testing Notes

Two files, and only one of them runs:

- **`tests/server/igmp/packet_codec_test.rs` — unprivileged, 16 tests, always runs.** The
  emitted packets field by field against RFC 2236 §2, the checksum (including a single-bit-flip
  sweep and the empty-slice guard), `igmp_payload` against malformed IP headers, the RFC
  destinations, and every rejection path of `execute_action`. This is the whole of the evidence
  an ordinary test run produces for this protocol.
- **`tests/server/igmp/e2e_test.rs` — all four cases `#[ignore]`d behind root.** They have never
  run in CI or on a developer machine. See `tests/server/igmp/CLAUDE.md`, and treat any claim
  they make about behaviour as untested.

Key testing considerations:

- Manual IGMP packet construction required
- Test on loopback or isolated network
- May require root privileges for raw sockets
- Multicast routing must be enabled on test interface

## References

- RFC 1112: Host Extensions for IP Multicasting
- RFC 2236: Internet Group Management Protocol, Version 2
- RFC 3376: Internet Group Management Protocol, Version 3
- RFC 4604: Using Internet Group Management Protocol Version 3 (IGMPv3) and Multicast Listener Discovery Protocol
  Version 2 (MLDv2) for Source-Specific Multicast

## LLM failure policy: silence, classified in the log

IGMP has no error message. The only packets a host may emit are a Membership Report and a
Leave Group, and a Report is a *positive claim* — "this host is a member of group G" — which
makes the querier forward that group's traffic onto the segment. Emitting one because netget
could not reach its backend would assert a membership nobody asked for; a Report or Leave that
netget merely observed has no spec-mandated response at all. So on backend failure the wire
stays silent, exactly as it does when no membership policy is configured. A wrong reply here is
worse than no reply, which is why this protocol does not follow the `http`/`tcp` shape of
answering with an error category.

The operator still has to be able to tell the cases apart, so each is tagged in the log
(`mod.rs`, the event task):

| tag | meaning |
|---|---|
| `decision=bad_checksum` | the message failed its own RFC 1071 checksum and was dropped before any decision was reached (WARN) — not a policy outcome, a corrupt packet |
| `decision=static_no_policy` | no instruction and no handler — static default, no LLM call |
| `decision=model_ignore` | the model explicitly chose `ignore_message` (`ActionResult::NoAction`) |
| `decision=model_no_answer` | the model answered, but produced no packet (WARN) |
| `decision=fail_closed_silent` | the LLM call errored (ERROR), with `category=overloaded\|unavailable` from `WireFailure::classify` |

The error text is logged only. Nothing derived from it can reach the socket, because nothing is
written to the socket on this path at all.
