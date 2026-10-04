# BMP tests

Peers: `python3 tests/server/bmp/install_peers.py ROOT` builds GoBGP 4.9.0 (`gobgpd`, `gobgp`)
and gobmp 1.1.0 from `tools/` (pinned by go.sum) and prints `NETGET_BMP_GOBGPD`,
`NETGET_BMP_GOBGP` and `NETGET_BMP_GOBMP`. `tests/helpers/bmp.rs` holds the collector policy
(close a router named "blocked", keep the rest) and the GoBGP configurations.

- `peer_test.rs` — a GoBGP route source (AS 65002, passive) and a GoBGP router (AS 65001)
  exporting BMP to NetGet: Initiation, Peer Up with both OPENs and ports, Route Monitoring for
  routes the test adds through the gobgp CLI (communities, MED) and one it withdraws, Statistics
  (GoBGP's 15 s minimum), Peer Down when the source is killed.
- `wire_test.rs` — per-peer header and distinguisher round trips; NetGet's exporter to NetGet's
  collector with every message type and exact values; refusals (no Initiation first, version 2,
  oversized, truncated per-peer header, a rejected router).

`tests/client/bmp/peer_test.rs` — NetGet's exporter against gobmp.
