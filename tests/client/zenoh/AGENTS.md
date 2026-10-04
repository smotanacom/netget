# Zenoh client tests

`peer_test.rs` runs zenoh-pico 1.10.1 examples as peers listening on their own ports. NetGet's
client connects to each as a peer: a pico subscriber receives what NetGet publishes, a pico
queryable answers NetGet's get, and NetGet's subscriber receives a pico publisher's values.
Needs `NETGET_ZENOH_PICO` from `tests/server/zenoh/install_peers.py`.
