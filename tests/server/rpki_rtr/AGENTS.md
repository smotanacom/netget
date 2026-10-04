# RPKI-RTR tests

Peers (`install_peers.py ROOT`, needs Go ≥ 1.24, cmake, a C compiler) print
`NETGET_STAYRTR`, `NETGET_RTRDUMP` and `NETGET_RTRCLIENT`.

```bash
./cargo-isolated.sh test --no-default-features --features tcp,rpki_rtr \
    --test server --test client -- rpki_rtr:: --test-threads=100
```

- `peer_test.rs` — two independent routers against NetGet's cache, unchanged:
  StayRTR 0.6.4 `rtrdump` (Go) resets over version 1 and version 0 (session id, serial and
  every VRP read back from its JSON dump), serial-queries for the delta, and is answered Cache
  Reset for a foreign session id without a handler call; RTRlib 0.8.0 `rtrclient` (C) stays
  connected on a pty, synchronizes, then follows a Serial Notify sent through
  `send_to_peer` to an incremental update whose `+`/`-` lines prove the withdrawal.
- `session_test.rs` — NetGet router ↔ cache: notify-driven and injected incremental updates,
  Cache Reset recovery, version 0 without timers; the cache's Error Report codes (4 in
  version 1, 5, 3, 0, 8); No Data Available keeps the session; a serial going backwards and
  an unreachable model are Internal Error; stopping the cache with a reset parked for a human
  closes the router and retires the intercept; the router's refusals of out-of-order and
  contradictory cache PDUs (0, 6, 8, 3).
- `codec_test.rs` — literal bytes for every PDU in both versions, host bits and the /32 and
  /128 boundaries, version/length/type checks, RFC 1982 serials across the wrap, batch rules,
  the frame bound and fragmented reads.

The client against StayRTR's cache is in `tests/client/rpki_rtr/peer_test.rs`.
