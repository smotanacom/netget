# Soulseek central service client verification

Independent peer: aioslsk 1.6.4. Setup: `tests/peers/bootstrap-p2p.sh` and `tests/peers/README.md`. Tests fail if required peer dependencies are absent; none is silently skipped.

Run `cargo test --no-default-features --features soulseek --test server --test client -- soulseek::`. The repo's serialized build wrapper may be used instead of Cargo. Standalone gate: `cargo check --no-default-features --features soulseek --all-targets`. Blocking clippy gates are part of protocol-pairs CI.

Selected legacy login, public room list/join/leave/chat, user status, peer endpoint lookup and search announcements. Login and data decisions come from handlers.

Experimental local server simulator; no account database, public service replacement, distributed search tree or obfuscation. No automatic peer dialing or real central multi-user room routing. Selected client can decode populated public rooms.

Lifecycle and malformed-wire checks are separate from the independent peer exchange. Fixtures use isolated temporary sessions and loopback addresses. Independent peers are external test processes and are never linked into NetGet.
