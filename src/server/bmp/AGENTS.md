# BMP collector — Experimental

BGP Monitoring Protocol version 3 (RFC 7854), with the Adj-RIB-Out flag of RFC 8671 and Loc-RIB
peers of RFC 9069. The `bmp` feature enables `bgp`: embedded BGP PDUs are framed by their own
length and decoded by netgauze through `crate::server::bgp::wire`. `codec.rs` holds framing, the
per-peer header, TLVs, the JSON each message becomes, and the builders the exporter uses.

Rust owns:
- Framing: version 3 only; a message of more than 256 KiB is refused from its header; once a
  message has started the rest must arrive within 60 s (between messages a router may be silent
  indefinitely — BMP has no keep-alive). The first message must be an Initiation.
- Decoding every message type into an event: `bmp_initiation` (sys_name, sys_descr, strings),
  `bmp_peer_up` (peer, local address and ports, both OPENs), `bmp_route_monitoring` (the UPDATE as
  JSON, or `decode_error`), `bmp_statistics` (counters by IANA name; per-AFI/SAFI gauges with afi
  and safi), `bmp_peer_down` (reason, NOTIFICATION or FSM event), `bmp_termination`,
  `bmp_route_mirroring`. Every event after Initiation carries `router` (the sysName, else the
  address). The per-peer header gives type, address (v4 or v6), ASN, BGP ID, distinguisher,
  post-policy, Adj-RIB-Out and the AS-path width, which decides how AS_PATH is decoded.
- A malformed message closes the session (`decision=protocol_refusal`).

The handler answers each event with `bmp_continue` or `bmp_close`; no answer closes the session.
A collector never writes, so it is `deliberately_silent` and `request_only`. Unknown message types
are skipped. No storage: the handler keeps what it needs in memory or SQLite.
