# BGP Client Implementation

BGP-4 (RFC 4271) monitoring client. It establishes a session with a peer and reports what the
peer says; it maintains no RIB and announces no routes.

**Status**: `Experimental`. **Port**: connects to TCP 179 by convention; no privilege needed.
**Stack**: `ETH>IP>TCP>BGP`.
**Spec**: [RFC 4271](https://datatracker.ietf.org/doc/html/rfc4271),
[RFC 6793](https://datatracker.ietf.org/doc/html/rfc6793) (four-octet AS).

## Wire format: shared with the server, not reimplemented

Every byte is encoded and decoded by **`src/server/bgp/wire.rs`**, which wraps
`netgauze-bgp-pkt`. Both halves live behind the same `bgp` Cargo feature, so `use
crate::server::bgp::wire` needs no additional gating and there is no second copy to keep in
sync. Read that module's header comment for why netgauze rather than a hand-rolled codec, and
for why KEEPALIVE and NOTIFICATION are deliberately hand-encoded there.

This client used to hand-roll its own encoder and decoder, and it still carried the defects the
server half had already had fixed:

| Was | Now |
|---|---|
| `&header_buf[0..16] != &BGP_MARKER` — sliced before any length check | `wire::parse_header` takes a `[u8; 19]`, so the bound is in the type; it also rejects a length outside `[19, 4096]` and below the per-type minimum *before* a body buffer is allocated |
| `local_as as u16` — AS 4200000000 became AS 60416, a different valid-looking ASN, with no capability and no diagnostic | `wire::build_open` puts AS_TRANS (23456) in the two-octet field and the real ASN in capability 65 (RFC 6793) |
| Peer ASN read from the two-octet field only, so a four-octet peer was recorded as AS 23456 | `open.my_asn4()` — the capability value when the peer sent one |
| AS_PATH always decoded as two-octet | The negotiated `peer_asn4` is threaded into `wire::decode` |
| UPDATE delivered to the model as `update_data_hex` | `wire::update_to_json` — `nlri`, `withdrawn_routes`, `origin`, `next_hop`, `as_path`, `path_attributes`, `end_of_rib` |
| Established declared as soon as *we* sent a KEEPALIVE | Established on the peer's KEEPALIVE, per RFC 4271 §8.2.2 |
| No keepalive cadence: one was sent only in reply to one | A ticker at `ceil(hold/3)` seconds, started at OpenConfirm and aborted when the read loop ends |
| No hold-timer enforcement: a silent peer held the socket until TCP noticed | The same task checks silence each tick and, on expiry, writes NOTIFICATION 4/0 and raises a shutdown `Notify` the read loop selects on |
| `send_notification` defaulted a missing `error_code` to 6 (Cease) | The parameter is required and range-checked; a malformed action is a failed action, not an invented teardown |
| `disconnect` closed the socket while its own description promised a Cease NOTIFICATION | Sends NOTIFICATION 6/2 (Administrative Shutdown), then closes — `ClientActionResult::Multiple`, which the executor used to log and discard |
| The LLM call held the `ClientData` mutex across the await, and the success arm reacquired it | Memory is copied out and the guard dropped before the call |

That last one is a latent deadlock rather than a live one: `call_llm_for_client` hardcodes
`memory_updates: None` (`src/llm/action_helper.rs`), so the reacquiring arm is unreachable
today. It would deadlock the read loop the moment client memory updates are implemented.

## Session

```text
TCP connect  -> our OPEN                     -> OpenSent
peer OPEN    -> validate, negotiate, KEEPALIVE -> OpenConfirm
peer KEEPALIVE                                -> Established -> bgp_connected
peer UPDATE                                   -> bgp_update_received
peer NOTIFICATION                             -> bgp_notification_received, close (never answered)
hold timer expires                            -> NOTIFICATION 4/0, close
```

### Timers

Both live in one task spawned at OpenConfirm, mirroring the server's `spawn_timers`. It has to
be a separate task for the same reason: the read loop can be parked in an LLM call, and a hold
timer that only ticks between model calls is not a timer.

- **Keepalives** go every `ceil(hold/3)` seconds, minimum one (RFC 4271 section 10).
- **Hold-timer expiry** (sections 4.2 and 6.5) is checked on the same tick: if nothing has been
  received for the negotiated hold time, the task writes NOTIFICATION 4/0 (Hold Timer Expired)
  itself — the read loop may be mid-LLM-call and the NOTIFICATION has to precede the close — and
  raises `Shutdown`.
- **A hold time of 0 disables both.** No ticker is spawned at all; a zero-second interval would
  busy-loop and expire immediately.
- **Before the peer's OPEN there is no ticker yet**, so the read loop applies its own deadline
  (`OPEN_HOLD_TIME`, 240s — the value RFC 4271 section 8.2.2 suggests for the Hold Timer in
  OpenSent). `spawn_timers` is reached only from the OPEN handler, so until this existed the one
  phase the peer controls entirely was covered by no timer at all: a peer that accepted the
  connection and then said nothing parked `read_exact` indefinitely.

**Both reads are under the select, and that was not always true.** The header read was; the
*body* read was a bare `read_exact`. A peer that sent a valid 19-octet header announcing a
4096-octet message and then stopped parked there forever, and hold-timer expiry could not break
it out — `Shutdown` was raised, the NOTIFICATION went out, and the read loop never noticed. Half
a message is not worth preserving, so abandoning a cancelled body read is fine: every path out
of that select ends the session.

`Shutdown` is a copy of the server's: an `AtomicBool` plus a `Notify`. The flag is the source of
truth, because `notify_waiters` only wakes tasks already waiting and a signal raised during an
LLM call would otherwise be lost. It is what makes any of this possible on the client, where the
read loop parks in a blocking `read_exact` that no ticker can interrupt: the loop selects over
the `Notify` and the read, so an expiry preempts a blocked read. `disconnect` raises the same
signal — before it did, that action stopped the action loop and left the read loop running.

### Teardown

The read loop's `JoinHandle` **and** the timer task's are both registered with
`AppState::register_client_task`, so `remove_client` aborts both. Registering the timer matters:
aborting a task does not abort tasks it spawned, so an unregistered timer would outlive the
client while holding the write half — and with it the socket — open. On normal exit the read
loop aborts the timer by its `AbortHandle`, shuts the write half down explicitly rather than
waiting for the last `Arc` clone to drop, and marks the client `Disconnected`.

(The root CLAUDE.md's blanket statement that a client's `JoinHandle` is never stored predates
`register_client_task` and is not true of this client.)

### Dashboard injection: command channel

`command_support::register_command_channel` runs before the read loop is spawned — and so
before the `bgp_connected` LLM call, which a manual `*` rule can park. `read_exact` is not
cancellation-safe, so commands are drained by a separate task (registered with
`register_client_task`, alongside the read loop and the timer task) that shares the write half.
Every wire verb here yields `SendData` / `Disconnect`, so the generic
`handle_stream_client_command` covers the whole vocabulary with no bespoke arm: an injected
`disconnect` writes the Cease NOTIFICATION, then the task raises `Shutdown` and half-closes, and
the read loop runs its normal teardown. That teardown calls `remove_client_handle` on every
exit, so the rail stops offering `[ send ]` on a dead session.

### Negotiation

- **Hold time**: `min(ours, theirs)`; zero on either side disables both timers. Keepalives go
  at `ceil(hold/3)` seconds, minimum one.
- **Four-octet AS**: the capability is always advertised with the real ASN. The peer's real
  ASN comes from *its* capability when present, and the peer's support decides how AS_PATH in
  an inbound UPDATE is decoded.

Protocol validity is decided in Rust and never delegated to the model: AS 0, a hold time
between 1 and 2 seconds, a bad version and an invalid BGP Identifier all earn a NOTIFICATION
without a model call.

## Events and actions

| Event | Fires | Reply path |
|---|---|---|
| `bgp_connected` | session reached Established | `wait_for_more`, `send_keepalive`, `send_notification`, `disconnect` — all reach the socket |
| `bgp_update_received` | peer announced or withdrew routes | the same four, all reaching the socket. No route can be announced back — there is no such action — but the session verbs work |
| `bgp_notification_received` | peer is tearing down | none; RFC 4271 forbids answering a NOTIFICATION. Anything returned is logged as discarded rather than silently dropped |

**`bgp_update_received` used to have no reply path**, on the reasoning that "this client
announces nothing, so an UPDATE handler cannot reply on the wire". That conflates announcing
*routes* with answering at all: the whole vocabulary here is `send_keepalive` /
`send_notification` / `disconnect` / `wait_for_more`, none of which is a route announcement, and
tearing the peering down over a prefix the operator will not accept is the obvious thing a
monitor wants to do with an UPDATE. So every action a handler returned for that event was
executed nowhere — the "asks the model what to do and then throws the answer away" defect from
the root CLAUDE.md, softened only by a `warn!`. `tests/client/bgp/update_reply_test.rs` is the
regression test, with a `wait_for_more` control so the teardown case proves something.

`get_async_actions` and `get_sync_actions` return the same list, and a client **cannot** narrow
per event even if it wanted to: `client_llm_action_set`
(`src/llm/actions/client_trait.rs`) offers the model `get_async_actions` ∪ `get_sync_actions` ∪
the firing event's own actions, because `call_llm_for_client` serves both the initial
instruction and every network event through one entry point. Two consequences worth knowing:

- Declaring the list twice is redundant rather than load-bearing. (It was load-bearing once —
  the tool list really was built from `get_async_actions` alone, which is how `wait_for_more`
  came to be invisible — but that was fixed centrally and this file described the old
  behaviour for some time afterwards.)
- `with_no_actions()` on `bgp_notification_received` does **not** stop the four verbs being
  offered for it. It records the intent; the read loop still has to log whatever comes back as
  discarded rather than assume it cannot happen.

## Startup parameters

| Name | Default | Validation |
|---|---|---|
| `local_as` | 65000 | 1-4294967295; 0 rejected. Above 65535 travels in the four-octet-AS capability |
| `router_id` | 192.168.1.100 | IPv4 dotted quad, not 0.0.0.0 |
| `hold_time` | 180 | 0 (timers off) or >= 3, at most 65535 |

All three are validated before the TCP connect, so a bad value fails the `open_client` with a
message naming the parameter rather than becoming a different valid-looking value on the wire.

## Limitations

1. **No RIB** and no route announcement — use the BGP server for that.
2. **No route filtering**: every UPDATE is reported.
3. **Route refresh** is not advertised and an inbound ROUTE-REFRESH is ignored (there is no RIB
   to re-advertise from). Graceful restart and add-path are not implemented.
4. **MP-BGP** (RFC 4760) parses — netgauze handles it — but MP_REACH/MP_UNREACH are reported by
   attribute name only, not projected into a shape a model can reason about.
5. **No authentication**: no TCP-MD5 or TCP-AO.
6. **Hold-timer resolution is one tick.** Expiry is checked every `ceil(hold/3)` seconds against
   whole-second timestamps, so the session is dropped somewhere in `[hold, hold + hold/3 + 1]`
   seconds of silence rather than at exactly `hold`. The server has the same behaviour. RFC 4271
   does not require sub-second precision here, but a peer with a 3s hold time may see up to ~4s.
7. **Never peered against a real daemon.** No `bgpd`/`bird`/`frr`/`gobgp` is installed on the
   development machine; the E2E tests drive NetGet's own BGP server.

## Testing

`tests/client/bgp/` — see its CLAUDE.md. Two mocked E2E tests driving a mocked NetGet BGP server
over a real socket, plus two suites that drive this client directly against a raw socket peer
that speaks BGP by hand:

- `hold_timer_test.rs` — one test goes silent and asserts the NOTIFICATION octets and the close,
  one keeps keepaliving and asserts the session survives past two hold times.
- `update_reply_test.rs` — a static `bgp_update_received` handler answering `disconnect` puts
  Cease / Administrative Shutdown on the wire and ends the session, with a `wait_for_more`
  control that must leave it up. No model is involved: the answer comes from
  `try_execute_client_event_handler`, which runs before the budget is debited.

## References

- RFC 4271: A Border Gateway Protocol 4 (BGP-4)
- RFC 4760: Multiprotocol Extensions for BGP-4
- RFC 6793: BGP Support for Four-Octet Autonomous System (AS) Number Space
