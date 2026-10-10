# BFD client (active role)

One session with one router, run by the server's engine (`src/server/bfd/`: `session.rs`,
`runner.rs`, `packet.rs`, `ttl.rs`). Read that AGENTS.md for the protocol details.

## Addresses

- `remote_addr` is `ip` or `ip:port`. With a bare IP the port is 3784, or 4784 when
  `multihop` is set.
- `multihop` defaults to true when the port is 4784.
- The router sends to its configured neighbour address at the BFD port. So the client listens
  on `local_address:<port>` with SO_REUSEADDR, beside any daemon holding the wildcard.
- It sends from `local_address` on a source port in 49152–65535, with TTL 255.
- `local_address` defaults to the source address of the route to the router.
- Packets from other addresses, naming another discriminator, or (single-hop) with a TTL
  other than 255 are dropped.

## Model

- `bfd_session_started` comes once; `bfd_session_state` comes on each change.
- Answers are `bfd_set_timers`, `bfd_admin_down`, `bfd_admin_up` and `disconnect`. Chains are
  bounded at `MAX_FOLLOWUP_DEPTH` (8).
- Injected actions (`send_to_client`) return the session snapshot as the `Executed` detail
  after the packet is sent, or `Rejected` for an invalid action.
- `disconnect` sends AdminDown, so the router sees the session taken down rather than timing
  out, then stops every task.
