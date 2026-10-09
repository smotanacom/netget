# ADC / ADCS hubs client verification

Independent peer: ncdc 1.25 and uhub 0.8.0. Setup: `tests/peers/bootstrap-p2p.sh` and `tests/peers/README.md`. Tests fail if required peer dependencies are absent; none is silently skipped.

Run `cargo test --no-default-features --features adc --test server --test client -- adc::`. The repo's serialized build wrapper may be used instead of Cargo. Standalone gate: `cargo check --no-default-features --features adc --all-targets`. Blocking clippy gates are part of protocol-pairs CI.

Anonymous BASE/TIGR negotiation, CID/PID validation, connection-scoped identity, handler-approved chat, search/results and DCTM/DRCM routing.

No GPA/PAS password login, UDP search or NAT traversal. ADCS uses implicit TLS with certificate validation.

Lifecycle and malformed-wire checks are separate from the independent peer exchange. Fixtures use isolated temporary sessions and loopback addresses. Independent peers are external test processes and are never linked into NetGet.
