# Soulseek peer browsing — server

Status: Experimental. Cargo feature: `soulseek_peer`. Both roles are registered through the protocol registries.

Selected P connection PeerInit, zlib-compressed shared directory listings and user information, both listening and connecting roles.

1 MiB compressed/decoded cap, 128 directories/files per collection. No F file-transfer channel, D distributed tree, pictures or obfuscation. This role browses metadata; it does not transfer Soulseek file contents.

The codec lives in `src/server/soulseek_peer/codec.rs`; the connecting role uses its Scanner. Each action is declared in the role's `actions.rs`. Shared `p2p_support` owns TCP/TLS tasks, connection admission (256), frame deadlines and bounded handler/notification queues (32). First connection read: 30 seconds; idle: 600 seconds; handshake/command/frame/write: 10 seconds. Handler time is excluded from wire deadlines; owner shutdown cancels it. Malformed/truncated/oversized framing closes the transport. There is no persistent protocol content or account store.

Optional implicit TLS is configured with `use_tls`; listeners require PEM `cert_path`/`key_path`. Clients validate certificates using public roots and optional PEM `ca_path`, and can override the expected DNS `server_name`. Raw TCP wrapping for protocols without a standardized TLS form is a local testing facility.

Tests use the independent aioslsk 1.6.4 stack, not the NetGet role pair as interoperability evidence. See `tests/server/soulseek_peer/AGENTS.md`. The roadmap records completed release gates; metadata remains Experimental.

Specification: https://nicotine-plus.org/doc/SLSKPROTOCOL.html
