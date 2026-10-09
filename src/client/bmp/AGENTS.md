# BMP exporter — Experimental

Uses `src/server/bmp/codec.rs` and the bgp feature's netgauze builders. On connect it sends an
Initiation (`sys_name`, `sys_descr`) and reports `bmp_connected`. Actions build one message
each: `bmp_peer_up` (both OPENs from the ASNs and identifiers; the peer is remembered by address),
`bmp_route_monitoring` (IPv4 announcements with next hop, AS path, origin, MED, local preference
and communities, and/or withdrawals, AS path width from the peer's `four_octet_as`),
`bmp_statistics` (counters by IANA name or number; 32-bit counters, 64-bit gauges, per-AFI/SAFI
gauges), `bmp_peer_down` (reason with NOTIFICATION or FSM event; forgets the peer) and
`bmp_termination` (then closes). A malformed action, or one naming a peer that is not up, is
refused before anything is written. When the collector closes, `bmp_collector_closed` is raised.
Not implemented: Route Mirroring, IPv6 routes.
