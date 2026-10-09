# RPKI-RTR router — Experimental, RFC 8210 (v1) and RFC 6810 (v0)

The router side. Rust drives the protocol; the handler hears about it:

- On connect Rust sends a Reset Query in the configured `version` (1 default, or 0; fixed
  for the session).
- At every refresh interval (from a version-1 End of Data, else `refresh_interval_secs`) and
  on a Serial Notify for the current session, Rust sends a Serial Query.
- Cache Reset → Rust forgets the session and sends a Reset Query, then raises
  `rpki_rtr_cache_reset`.
- No Data Available → `rpki_rtr_error_report`, retry after the retry interval; every other
  Error Report ends the session (no reconnect).

`rpki_rtr_synchronized` reports each completed exchange: kind (reset/incremental), session,
serial, announced/withdrawn counts, up to 256 records with their announcement flag,
`truncated`, timers, and how many Router Key PDUs were ignored. There is **no VRP table** and
no route-origin validation; persist with memory or SQLite if a scenario needs state.

A dedicated registered reader task reads PDUs (header unbounded in time — a cache is silent
between polls — body bounded), so timers and commands never cancel a half-read PDU. The
router refuses, with the RFC code, a Prefix outside an exchange (0), a withdrawal during a
reset (6), a PDU in another version (8), a query from the cache (3), a session id that changes
inside an exchange or at End of Data (0), and a serial that goes backwards (0). One exchange
at a time; `exchange_timeout_secs` (120) bounds query → End of Data; 1,000,000 PDUs per
exchange.

Actions: `rpki_rtr_reset_query`, `rpki_rtr_serial_query`, `disconnect`. Not implemented:
SSH/TLS transports, Router Key/ASPA processing, reconnect after the session ends.
