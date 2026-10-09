# BMP client tests

`peer_test.rs` runs gobmp 1.1.0 (independent, unchanged), which parses and prints every message
it collects. NetGet's exporter reports a peer up (both OPENs, ports), announcements with AS path,
origin, MED, local preference and a community, a withdrawal, statistics, a peer down with its
NOTIFICATION and a termination, and is refused a route for a peer that is not up. Needs
`NETGET_BMP_GOBMP` from `tests/server/bmp/install_peers.py`.
