# WAMP client tests

`peer_test.rs` runs the nexus 3.3.0 router (realm1; independent, unchanged) with a local nexus
session that registers `com.example.add2`, publishes `com.example.tick`, subscribes to
`com.example.fromnetget` and keeps calling `com.example.netget.echo`. NetGet's client calls add2
(a result and a callee error), subscribes to the ticks, publishes to nexus's subscriber,
registers the echo procedure that nexus then calls (answered by the handler script), and leaves.
Assertions read nexus's own output as well as NetGet's events. Needs `NETGET_WAMP_NEXUS` from
`tests/server/wamp/install_peers.py`.
