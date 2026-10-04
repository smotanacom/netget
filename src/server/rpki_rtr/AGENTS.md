# RPKI-RTR cache — Experimental, RFC 8210 (v1) and RFC 6810 (v0)

`rpki_rtr` is the cache side of the RPKI-to-Router protocol on TCP 323. Rust owns framing,
the cache session id (`session_id` startup parameter, else random per start), the timers sent
in version-1 End of Data, version negotiation and every Error Report. The handler owns the
data: there is no RPKI repository, validation engine or VRP store in Rust.

## Files

- `codec.rs` — every RFC 8210/6810 PDU except Router Key and ASPA; version-specific End of
  Data lengths; prefix/max-length/host-bit rules (a /32 or /128 used to overflow a shift);
  RFC 1982 serial order; one bounded frame read (header checked before the body is
  allocated, 4096 bytes max) that does not care how TCP chunks the bytes; RFC 8210 §12 error
  names. Shared with the client.
- `actions.rs` — `rpki_rtr_response` (sync), `rpki_rtr_serial_notify` and `disconnect` (peer
  actions), events `rpki_rtr_reset_query` and `rpki_rtr_serial_query`.
- `mod.rs` — accept loop (256 connections), one owned task per router, a registered peer
  command task for Serial Notify/disconnect sharing the connection's writer.

## Session rules Rust applies without the handler

- The router's first PDU fixes the session's version. Version > 1 → Error Report 4
  (Unsupported Protocol Version) sent in version 1, then close, so the router can retry lower.
  A later PDU in another version → Error Report 8.
- Serial Query for another session id → Cache Reset, no handler call.
- A router sending a cache PDU → Error Report 3; an unknown PDU type → 5; a malformed or
  over-long PDU → 0 (Corrupt Data). A router's Error Report ends the session.

## Handler contract

`rpki_rtr_response` takes exactly one of: `serial` + `records`, `cache_reset: true` (serial
queries only), `no_data: true` (Error Report 2, non-fatal — the session stays open). Records
are `{prefix, max_length, asn, announcement}`; a reset answer cannot withdraw, and a serial
answer must not go backwards from the router's serial. At most 4096 records per answer and a
1 MiB action. Any failure — backend error, silence, an invalid or contradictory answer — is
Error Report 1 (Internal Error, "try again later"), never invented data; the log carries the
`decision=` tag.

Serial Notify is sent with `send_to_peer(server, connection, {"type":"rpki_rtr_serial_notify",
"serial": N})` (or the dashboard's peer row) in the session's version and session id.

## Not implemented

SSH/TLS transports (RFC 8210 §9), Router Key and ASPA PDUs, any VRP storage, ROV.
