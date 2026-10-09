# Gnutella 0.6 peers server verification

Independent peer: gtk-gnutella 1.3.1. Setup: `tests/peers/bootstrap-p2p.sh` and `tests/peers/README.md`. Tests fail if required peer dependencies are absent; none is silently skipped.

Run `cargo test --no-default-features --features gnutella --test server --test client -- gnutella::`. The repo's serialized build wrapper may be used instead of Cargo. Standalone gate: `cargo check --no-default-features --features gnutella --all-targets`. Blocking clippy gates are part of protocol-pairs CI.

Three-way TCP handshake and selected ping/pong, query/query-hit and push descriptors; handlers supply endpoints and search results.

64 KiB payload, 32 hits, TTL+hops <=16. No ultrapeer routing, QRP, GGEP interpretation, DHT, public network traversal, HTTP file downloads or push dialing. Push is a descriptor notification.

Lifecycle and malformed-wire checks are separate from the independent peer exchange. Fixtures use isolated temporary sessions and loopback addresses. Independent peers are external test processes and are never linked into NetGet.
