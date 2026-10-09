# Soulseek peer browsing client verification

Independent peer: aioslsk 1.6.4. Setup: `tests/peers/bootstrap-p2p.sh` and `tests/peers/README.md`. Tests fail if required peer dependencies are absent; none is silently skipped.

Run `cargo test --no-default-features --features soulseek_peer --test server --test client -- soulseek_peer::`. The repo's serialized build wrapper may be used instead of Cargo. Standalone gate: `cargo check --no-default-features --features soulseek_peer --all-targets`. Blocking clippy gates are part of protocol-pairs CI.

Selected P connection PeerInit, zlib-compressed shared directory listings and user information, both listening and connecting roles.

1 MiB compressed/decoded cap, 128 directories/files per collection. No F file-transfer channel, D distributed tree, pictures or obfuscation. This role browses metadata; it does not transfer Soulseek file contents.

Lifecycle and malformed-wire checks are separate from the independent peer exchange. Fixtures use isolated temporary sessions and loopback addresses. Independent peers are external test processes and are never linked into NetGet.
