# NMDC file peers client verification

Independent peer: ncdc 1.25. Setup: `tests/peers/bootstrap-p2p.sh` and `tests/peers/README.md`. Tests fail if required peer dependencies are absent; none is silently skipped.

Run `cargo test --no-default-features --features dc_peer --test server --test client -- dc_peer::`. The repo's serialized build wrapper may be used instead of Cargo. Standalone gate: `cargo check --no-default-features --features dc_peer --all-targets`. Blocking clippy gates are part of protocol-pairs CI.

Uploader/listener and downloader/connector, lock/key handshake, ADCGET/ADCSND binary file and compressed XML file-list downloads, bounded ranges and optional full-payload TTH verification.

1 MiB per transfer; identifiers cannot contain whitespace. No filesystem access, transfer scheduler, TTH leaf requests or automatic hub discovery. NMDC defaults to plaintext.

Lifecycle and malformed-wire checks are separate from the independent peer exchange. Fixtures use isolated temporary sessions and loopback addresses. Independent peers are external test processes and are never linked into NetGet.
