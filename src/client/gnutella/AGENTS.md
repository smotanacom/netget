# Gnutella 0.6 peers — client

Status: Experimental. Cargo feature: `gnutella`. Both roles are registered through the protocol registries.

Three-way TCP handshake and selected ping/pong, query/query-hit and push descriptors; handlers supply endpoints and search results.

64 KiB payload, 32 hits, TTL+hops <=16. No ultrapeer routing, QRP, GGEP interpretation, DHT, public network traversal, HTTP file downloads or push dialing. Push is a descriptor notification.

The codec lives in `src/server/gnutella/codec.rs`; the connecting role uses its Scanner. Each action is declared in the role's `actions.rs`. Shared `p2p_support` owns TCP/TLS tasks, connection admission (256), frame deadlines and bounded handler/notification queues (32). First connection read: 30 seconds; idle: 600 seconds; handshake/command/frame/write: 10 seconds. Handler time is excluded from wire deadlines; owner shutdown cancels it. Malformed/truncated/oversized framing closes the transport. There is no persistent protocol content or account store.

Optional implicit TLS is configured with `use_tls`; listeners require PEM `cert_path`/`key_path`. Clients validate certificates using public roots and optional PEM `ca_path`, and can override the expected DNS `server_name`. Raw TCP wrapping for protocols without a standardized TLS form is a local testing facility.

Tests use the independent gtk-gnutella 1.3.1 stack, not the NetGet role pair as interoperability evidence. See `tests/client/gnutella/AGENTS.md`. The roadmap records completed release gates; metadata remains Experimental.

Specification: https://rfc-gnutella.sourceforge.net/src/rfc-0_6-draft.html
