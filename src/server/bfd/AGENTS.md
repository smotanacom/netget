# BFD server (speaker, passive role)

RFC 5880 Asynchronous mode over UDP: RFC 5881 single-hop (port 3784) and RFC 5883 multihop
(port 4784). Hand-rolled. The client (`src/client/bfd/`) runs the same engine in the active
role.

## Files

- `packet.rs`: the Control packet and all five authentication types.
  - Decoding performs every check §6.8.6 allows without session state.
  - `verify` checks the password or digest.
- `session.rs`: the state machine and timers.
  - Desired Min TX is at least one second while not Up.
  - A change while Up starts a Poll Sequence. An increase of the transmit interval or a
    decrease of the receive interval waits for the Final.
  - Detection Time is the remote Detect Mult × max(local RequiredMinRx, remote DesiredMinTx).
  - When it expires: Down, diag 1, and the remote discriminator is forgotten.
  - Keyed authentication keeps a sequence window of 3 × Detect Mult. Meticulous types require
    a strictly increasing sequence number.
- `runner.rs`: one task per session.
  - Sends periodically with 75–100 % jitter (75–90 % at Detect Mult 1).
  - Answers a Poll with a Final at once.
  - Applies actions, putting the packet on the wire before replying to the caller.
  - Reports state changes as `Note`s.
  - Also has `tx_socket`: source port in 49152–65535 (RFC 5881 §4), TTL 255.
- `ttl.rs`: `IP_RECVTTL` and `recvmsg`, so single-hop packets with a TTL other than 255 are
  dropped (RFC 5881 §5, GTSM). Multihop does not check the TTL.

## Who decides what

Rust runs every packet; a model cannot keep a 300 ms clock. The model is asked twice:

- **`bfd_session_request`**: a peer with no session is sending Control packets. The answer is
  `bfd_accept_session` (optional timers, otherwise the server's startup timers) or nothing.
  - The key is verified *before* the model is asked, so a peer that cannot authenticate is
    never a request.
  - A declined peer is not asked about again for `DECLINE_HOLD` (30 s), because a Down peer
    sends once a second.
  - At most `max_sessions` sessions plus pending asks (default 64).
- **`bfd_session_state`**: a session changed state. The answer is `bfd_set_timers`,
  `bfd_admin_down`, `bfd_admin_up`, or nothing. Chains are bounded at `MAX_FOLLOWUP_DEPTH` (8).

Each session is a connection with a peer handle, so the dashboard's per-peer message (and MCP
`send_to_peer`) takes the same three session actions. The outcome is the session snapshot, or
`Rejected` for an invalid action.

A session that is Down with nothing heard for `SESSION_IDLE` (60 s) ends, and its connection
closes.

**Deliberately silent.** BFD has no refusal, so a peer that is not accepted (or a backend
failure) is never answered. The log says `decision=model_silent` / `fail_closed_llm_error` /
`fail_closed_action_error`.

As the passive role requires (§6.8.7), a session whose remote discriminator is zero (before
the first packet, and after the Detection Time expires) sends nothing.

## Addresses

- The server binds `host:port` with SO_REUSEADDR. A routing daemon on the same host holds the
  BFD port on the wildcard address, and a socket bound to a specific address still receives
  what is sent to that address.
- Replies go to the peer's IP at the server's own port number, because BFD's destination port
  is fixed per mode. They are sent from the bind address.
- A wildcard-bound server sends from the routed source address, so bind the address the peer
  has configured as its neighbour.
- `multihop` defaults to true when the port is 4784.

## Not implemented

- Demand mode and the Echo function. The D bit is never set, and Required Min Echo RX is
  always 0.
- IPv6 hop-limit checks.
- More than one key per session.
