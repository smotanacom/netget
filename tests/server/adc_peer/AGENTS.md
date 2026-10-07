# ADC / ADCS file peers server verification

Independent peer: ncdc 1.25. Setup: `tests/peers/bootstrap-p2p.sh` and `tests/peers/README.md`. Tests fail if required peer dependencies are absent; none is silently skipped.

Run `cargo test --no-default-features --features adc_peer --test server --test client -- adc_peer::`. The repo's serialized build wrapper may be used instead of Cargo. Standalone gate: `cargo check --no-default-features --features adc_peer --all-targets`. Blocking clippy gates are part of protocol-pairs CI.

CSUP/CINF negotiation, CGET/CSND file and compressed XML file-list downloads, bounded ranges and optional full-payload TTH verification.

1 MiB per transfer. No transfer scheduler or automatic hub discovery. Connector cid/token parameters link the session to handler-managed hub rendezvous; direct listener identification is not an authorization check. ADCS uses verified implicit TLS.

Lifecycle and malformed-wire checks are separate from the independent peer exchange. Fixtures use isolated temporary sessions and loopback addresses. Independent peers are external test processes and are never linked into NetGet.
