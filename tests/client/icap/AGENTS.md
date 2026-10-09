# ICAP client tests

`peer_test.rs` runs c-icap 0.6.5 (independent, unchanged) with its echo service from an owned
configuration and drives OPTIONS, RESPMOD without 204 (echoed whole), REQMOD (204 or echoed with the
request intact) and a 3000-byte body with a 1024-byte preview (continued, echoed whole). Needs
`NETGET_C_ICAP` from `tests/server/icap/install_peers.sh`; fails without it.
