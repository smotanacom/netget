# sFlow collector checks

`codec_test.rs` compares native encoding and decoding against a literal
four-sample-format oracle and explicit typed expectations. It checks every
truncation, malformed lengths/version/address type, declared bounds, unknown
record privacy, packet header summaries, cache transactionality/isolation,
sequence wrap/gaps/late/lower uptime and idle expiry. `e2e_test.rs` uses actual
UDP sockets for default no-model collection, silent terminal decisions,
malformed recovery, parsing while a handler is parked, capacity, owned socket
and intercept release, live idle expiry, shared memory and failed actions.

`peer_test.rs` requires the unmodified public Cistern encoder. Its 320-byte wire
output must exactly match `cistern_compact.hex`; native typed values and the
actual GoFlow2 service are checked separately. This verifies the explicitly
compensated public source-ID arguments against normative bytes and another
implementation. The peer's VLAN encoder is never used. Test-only peer commands
are in `peer.go`; unmodified implementation source remains outside the repository.

Bootstrap into owned temporary storage, using an existing Go toolchain (1.21+):

```sh
python3 tests/server/sflow/install_peers.py /tmp/netget-sflow-peers
export NETGET_SFLOW_PEER=/tmp/netget-sflow-peers/cistern-sflow-peer
export NETGET_SFLOW_COLLECTOR=/tmp/netget-sflow-peers/goflow2-v2.2.7
cargo test --locked --no-default-features --features sflow --test server --test client sflow::
```

Use the programme shared guard in expansion worktrees. The bootstrap supports
Linux amd64 and macOS arm64, fails elsewhere, bounds downloads, checks versions,
SHA256 and BSD licenses, and uses isolated owned Go caches with network module
fetches disabled. It installs no global toolchain/package. Cistern source is
`ed105e3cf9fb208505ed3a9939c9449321cbacf1`, archive SHA256
`79dd1073b1df3fa5fb3c7c13f8ec9e23c6f700a51a3355a5f316b1a473ab6e8e`.
GoFlow2 2.2.7 Darwin arm64 SHA256 is
`6bc188842983edbf2df26788180bce3ee801788fec3aebedcff1f5d96eb9fd57`;
Linux amd64 SHA256 is
`63d3bb6c4e458f56ae3268eac74979da1fee0cea5342362295e7d587784af14b`.
All peers are BSD-3-Clause and external executables; none is a runtime dependency.

Required peers fail when absent; no ignores or missing-peer skips. Shared fixture
and daemon readiness helpers are in `tests/helpers/sflow.rs`. Only fixture
readiness is retried; production exports are sent once. The collector is an
actual GoFlow2 service. GoFlow2 represents VLAN counters as opaque, so exact
normative bytes are checked; independent Cistern decoding supplies typed VLAN
evidence. No full-sFlow, other-platform daemon, fuzz or capture evidence is claimed.
