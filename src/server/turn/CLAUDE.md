# TURN Protocol Implementation

## Overview

TURN (Traversal Using Relays around NAT) server, RFC 8656. Clients that cannot reach a peer
directly ask a TURN server for a *relayed transport address*; the server then forwards
datagrams in both directions on their behalf.

**Compliance**: RFC 8656 (TURN), RFC 8489 (STUN). RFC 5766 is the obsolete predecessor.

> **Status: `Experimental` — it does relay.**
>
> Every granted allocation binds its own UDP socket. Peer traffic arriving there is
> forwarded to the client as a Data indication or a ChannelData frame; Send indications
> and ChannelData frames from the client are forwarded to permitted peers from that
> socket. `tests/server/turn/e2e_test.rs` proves this with two ordinary UDP sockets and
> a payload that has to cross in both directions.
>
> It is `Experimental`, not `Beta`: it has never been run against `turnutils_uclient`,
> libwebrtc or any other real TURN client, and it implements **no authentication**.

## Division of labour

This is the part to understand before changing anything here.

| | Owner | Why |
|---|---|---|
| Binding the relay socket, choosing the relay address | Rust | It is a fact about a socket, not a decision. A model-invented address is exactly the bug this protocol shipped with. |
| Granting Allocate / Refresh / CreatePermission / ChannelBind, lifetimes, which peers | LLM (or script / static handler) | This is policy, which is what NetGet exists to put a model in charge of. |
| Forwarding each relayed packet | Rust | One LLM round-trip per packet would be absurd, and the permission decision was already made on the control plane. |
| Binding requests (STUN over the TURN port) | Rust | The answer is the client's own address. |

**The data plane never raises an event and never calls the LLM.** Do not add one. Per-packet
messages must also stay off `status_tx`, which is an unbounded channel with no backpressure —
relay forwarding logs at `trace!` to the file log only.

**Static default when no operator policy is configured.** Whether to grant is policy, not
wire-determined. So with **no operator policy** — no server instruction and no per-event handler
(`should_call_llm` in `mod.rs` = `has_instruction || has_handler`) — every control request
(Allocate / Refresh / CreatePermission / ChannelBind) is **fail-closed (grant nothing) with no LLM
round-trip at all**. The model (or a script / static handler) is consulted **only when the operator
opts in** with the grant policy. `call_llm_for_event` returns `None` in the no-policy case, which is
the same signal it uses for a refusal or an LLM failure — nothing is granted.

**Three no-grant outcomes, kept apart.** Granting nothing is the same in all three, but what the
client is told is not, and the log tags them so an operator can tell them apart:

| Situation | Log | On the wire |
|---|---|---|
| No operator policy | `decision=fail_closed_no_policy` | **nothing** — the request is simply not served, and no LLM call is made |
| Backend errored | `decision=fail_closed_llm_error` (full error at `error!` + status stream) | STUN error response: **508** Insufficient Capacity when `WireFailure::Overloaded`, **500** Server Error otherwise |
| Model answered without a grant action | `decision=model_reject` | whatever the model's own actions emit (typically `send_turn_error_response`, or nothing) |

The error response carries `WireFailure::text()` as its reason phrase and nothing else. Never
interpolate the backend error, its URL, the model name or an `anyhow` chain into the reason: it
goes to a stranger on the network, and `tests/wire_failure_test.rs` fails the build on the known
leak idioms. `tests/server/turn/llm_failure_test.rs` pins both halves — that a reply arrives at
all (silence left the client retransmitting for the full ~39s STUN schedule), and that its reason
phrase is the category.

## Library choice

Hand-rolled on top of the STUN message format, *not* `webrtc-turn`, even though the `turn`
feature declares that dependency. `webrtc-turn` 0.1.3 has all the machinery (allocation
manager, relay address generators, request handler, its own client), and adopting it was the
first choice. Three things stopped it, in order of how hard they are to work around:

1. **Its public API is expressed in `webrtc-util` types it does not re-export.** Every entry
   point needs one: `Manager::new` takes a `Box<dyn RelayAddressGenerator>`, all three
   supplied generators need `net: Arc<util::vnet::net::Net>`, implementing the trait yourself
   needs `util::Conn` and `util::Error` in the signature, and `AuthHandler` needs
   `util::Error`. So adoption requires adding `webrtc-util` (and `webrtc-stun`) to
   `Cargo.toml` and to the `turn` feature.
2. **`Allocation::relay_addr` and `relay_socket` are `pub(crate)`.** A partial adoption — its
   allocation manager under NetGet's own request handling — cannot read back the address to
   report to the client, and cannot send anything client→peer.
3. **Its request handler mandates long-term credentials.** `authenticate_request` is called
   unconditionally for Allocate/Refresh/CreatePermission/ChannelBind and answers 401 before
   any event could be raised, so the model could not be the one to grant or refuse. Using
   `server::Server` wholesale also runs its own read loop, which would delete NetGet's event
   and action surface entirely.

If someone adds `webrtc-util` to the `turn` feature, point 1 dissolves; points 2 and 3 mean
the useful shape would still be "NetGet's loop + its `proto` encoders", not its server.

## Architecture

### Allocation state

```rust
struct TurnAllocation {
    client_addr:  SocketAddr,          // the client 5-tuple this belongs to
    relay_addr:   SocketAddr,          // advertised address (may differ from bound IP)
    relay_socket: Arc<UdpSocket>,      // the socket peers actually send to
    state:        Arc<Mutex<AllocationState>>,
    relay_task:   JoinHandle<()>,
}

struct AllocationState {   // shared with the relay task
    expires_at:  Instant,
    permissions: HashMap<IpAddr, Instant>,       // RFC 8656: per IP, 5 minutes
    channels:    HashMap<u16, (SocketAddr, Instant)>,  // 10 minutes
}
```

Two details that are load-bearing:

- **`expires_at` lives in the shared state, not beside the socket.** The relay task checks it
  per packet. Keeping it only in the allocation table meant an expired allocation kept
  relaying until the 30-second cleanup tick happened to notice.
- **`impl Drop for TurnAllocation` aborts `relay_task`.** Dropping a `JoinHandle` detaches the
  task rather than stopping it, so without this an expired, replaced, or server-stopped
  allocation would keep its socket bound and keep forwarding.

The table is `HashMap<allocation_id, TurnAllocation>` behind an `Arc<Mutex<_>>`; the cleanup
task holds only a `Weak` reference, because `register_server_task` stores one handle per
server (the accept loop's) and this task must notice on its own when the server stops.

### Allocate: reserve, then ask

1. Reject non-UDP `REQUESTED-TRANSPORT` with 442, and anything past `MAX_ALLOCATIONS` (256)
   with 508. Resource exhaustion is not a policy question.
2. **Bind the relay socket first**, then raise `turn_allocate_request` with its address in
   `relay_address`. The model echoes that value back in `send_turn_allocate_response`.
3. If the action names any other address, the allocation is refused with 508 and the model's
   packet is never sent. Confirming an address nobody listens on is worse than refusing.
4. If the model refuses, ignores, or the LLM call fails, the reserved socket is dropped and
   closed. Nothing is granted by default.

The lifetime the model grants is capped at `MAX_LIFETIME_SECONDS` (3600).

### Relay data flow

**Client → peer**: Send indication (or ChannelData frame) → look up the client's allocation →
check the peer is permitted → `relay_socket.send_to(payload, peer)`. RFC 8656 section 10.2:
indications are never answered with an error, so anything unrelayable is silently dropped.

**Peer → client**: the per-allocation relay task reads the relay socket → drops the packet if
the allocation has expired or the source IP is not permitted → wraps it in a ChannelData frame
if a channel is bound to that peer, otherwise in a Data indication (XOR-PEER-ADDRESS + DATA) →
sends it on the TURN socket to the client's address.

### Message parsing

`TurnMessage::parse` is the only decoder. It is fed unauthenticated datagrams from anyone who
can reach the socket, so every field is bounds-checked and it returns `Option` rather than
panicking — a panic in the per-datagram task is silent and the server keeps reporting Running.
It trusts the shorter of the declared attribute length and the bytes actually received, and
stops (keeping what it has) at the first attribute whose length runs past the end.

ChannelData frames are tested for **before** STUN parsing: they have no magic cookie, and
their first two bits are `01` (channel numbers are 0x4000–0x7FFF).

## LLM integration

### Events

| Event | Extra fields beyond the common set | Decision |
|---|---|---|
| `turn_allocate_request` | `relay_address`, `requested_lifetime_seconds`, `requested_transport` | grant / refuse, lifetime |
| `turn_refresh_request` | `requested_lifetime_seconds` | extend, refuse, or delete (lifetime 0) |
| `turn_create_permission_request` | `peer_addresses` | which of the requested peers to permit |
| `turn_channel_bind_request` | `channel_number`, `peer_address` | grant / refuse |

Common to all four: `transaction_id` (hex), `peer_addr`, `local_addr`, `message_type`,
`bytes_received`, `existing_allocations`. Static handlers echo the transaction ID with
`"transaction_id": "{{event.transaction_id}}"` and the relay address with
`"{{event.relay_address}}"` — no LLM call needed.

### Actions

- `send_turn_allocate_response` — `relay_address` (**must** be `{{event.relay_address}}`),
  `transaction_id`, optional `client_address` (echo `{{event.peer_addr}}`, sent as
  XOR-MAPPED-ADDRESS), `lifetime_seconds`, `allocation_id`.
- `send_turn_refresh_response` — `transaction_id`, `lifetime_seconds` (0 deletes).
- `send_turn_create_permission_response` — `transaction_id`, optional `peer_addresses` to
  permit a subset. Omitting it permits every peer the request named; peers the request did
  *not* name are ignored, so a hallucinated address cannot open a hole.
- `send_turn_channel_bind_response` — `transaction_id`. Binding also permits the peer.
- `send_turn_error_response` — `error_code`, `reason`, `transaction_id`, and `method`
  (`allocate` / `refresh` / `create_permission` / `channel_bind`). The method must match the
  request or the client discards the error.
- `ignore_request`.

Send and Data indications have **no event and no action**, by design (see the table above).

### Startup parameters

- `relay_ip` (optional) — the IP advertised in XOR-RELAYED-ADDRESS. Relay sockets are always
  bound to the server's own listen address; set this only when clients reach the relay at a
  different address (NAT, port forwarding). Binding to a wildcard address without setting it
  logs a WARN, because `0.0.0.0:port` is useless to a client.

## Limitations

1. **No authentication.** REALM / NONCE / MESSAGE-INTEGRITY are not implemented, so this is an
   open relay to anyone who can reach the port, bounded only by the model's grant decisions
   and the 256-allocation cap. Real deployments must not expose it. This is the single largest
   gap between this and a usable TURN server.
2. **Never tested against a real TURN client** (`turnutils_uclient`, libwebrtc, Pion). The E2E
   suite encodes and decodes the wire format itself.
3. **UDP relays only.** No TCP allocations, no `REQUESTED-ADDRESS-FAMILY` (IPv4 relay
   addresses in practice, since the relay binds the listen address).
4. **No RESERVATION-TOKEN, EVEN-PORT, DONT-FRAGMENT, BANDWIDTH or ALTERNATE-SERVER.**
5. **Datagrams larger than 2048 bytes are truncated** (`RELAY_MTU`), with a WARN when a
   received datagram exactly fills the buffer.
6. **Permission and channel lifetimes are fixed** at the RFC values (300s / 600s); the model
   cannot change them, only whether they exist.
7. **One allocation per client 5-tuple.** A second grant replaces the first (RFC 8656 says
   answer 437 Allocation Mismatch; a model can do that explicitly with
   `send_turn_error_response`).

## Security notes

**Open relay / amplification**: TURN relays are an amplification vector and, unauthenticated,
a free proxy. The cap and the per-IP permission checks limit the blast radius; authentication
is what would actually fix it.

**Fail-closed points** worth preserving if you refactor: no allocation without an explicit
grant action; no relaying to or from an unpermitted IP; a mismatched relay address refuses
rather than confirms; an LLM error grants nothing (and answers 500/508 rather than going
silent, without putting the error itself on the wire).

## Example prompts

```
Start a TURN relay server on port 3478 with 600 second allocations.
```

```
Start a TURN relay server on port 0 that rejects all allocations with error 508
Insufficient Capacity.
```

```
Start a TURN relay server on port 3478 that only permits peers in 198.51.100.0/24.
```

## References

- RFC 8656: Traversal Using Relays around NAT (TURN)
- RFC 8489: Session Traversal Utilities for NAT (STUN)
- `webrtc-turn` 0.1.3 source, for a second opinion on the wire format:
  `~/.cargo/registry/src/*/webrtc-turn-0.1.3/src/`
