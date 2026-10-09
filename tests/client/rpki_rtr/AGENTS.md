# RPKI-RTR router tests

`peer_test.rs` runs StayRTR 0.6.4 `stayrtr` (independent Go cache), unchanged, on a VRP file
the test rewrites. The router's reset brings both VRPs and StayRTR's `-rtr.refresh` timer;
after the rewrite StayRTR raises the serial and sends Serial Notify, and the router's own
Serial Query must return exactly one announcement and one withdrawal at serial + 1. A second
test negotiates version 0. Needs `NETGET_STAYRTR` from
`tests/server/rpki_rtr/install_peers.py`; fails without it. Pair and negative tests are in
`tests/server/rpki_rtr/session_test.rs`.
