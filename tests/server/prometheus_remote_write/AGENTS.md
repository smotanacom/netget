# Remote Write 1.0 receiver checks

`codec_test.rs` asserts a literal signed-time float protobuf/Snappy oracle in both
encoding and decoding directions. It covers signed i64 boundaries, finite/-0,
NaN/Inf/stale semantics, scalar/order/duplicate validation, all declared codec
bounds, protobuf truncations/wire types and field budget, Snappy literal/copy forms
and output bombs, unknown/reserved-field privacy and combined body budget.
`e2e_test.rs` checks actual HTTP defaults/no-model, route/method/header/version/MIME,
Bearer privacy, body limits, rejection statuses, successful empty/common handlers,
failed actions overriding acceptance, backend failure, parked removal, connection
and header caps/recovery and actual30s header/body deadlines preserving a parked
business request. Common-action effects before failure are deliberately asserted.

`peer_test.rs` runs actual official Prometheus3.15.0, whose real sender scrapes
values from official Python prometheus-client0.22.1. It emits version1.0
protobuf/Snappy with v2, metadata, exemplars and native histograms disabled. Tests
assert native typed Unicode labels/values, timestamps, NaN, Bearer exclusion and
its actual retry of the same initial batch after a503 receiver rejection.
The exporter is a scrape fixture, not the remote-write wire encoder. Native
framing is independent of both Apache-2.0 peer implementations.

Bootstrap only into programme-owned temporary storage:

```sh
python3 tests/server/prometheus_remote_write/install_peers.py /tmp/netget-prw-peers
export NETGET_PRW_PROMETHEUS=/tmp/netget-prw-peers/prometheus-3.15.0
export NETGET_PRW_PYTHON=python3
export PYTHONPATH=/tmp/netget-prw-peers/python
cargo test --locked --no-default-features --features prometheus-remote-write --test server --test client prometheus_remote_write:: -- --test-threads=100
```

Use the shared Cargo guard in programme worktrees. Linuxamd64 and macOSarm64
bootstrap are explicit; other targets fail. Tests fail if a required peer is
missing or wrongly versioned, never ignore or skip. An optional second bootstrap
argument explicitly reuses an already installed3.15.0 binary read-only with version
verification; this is not evidence of a release-archive binary hash match.
Default bootstrap verifies official archives before extracting only the binary,
LICENSE and NOTICE;128MiB download and256MiB individual extraction bounds. Owned
`RealServer` process groups, death ties and fresh temporary TSDB/config/logs ensure
cleanup on success, cancellation or panic. No global install, toolchain or container.

Official archive SHA256 pins (from GitHub release asset metadata):
Linuxamd64 `2a542df32eac02ee17b9d844fb2aa1de00dafa5476579ba8a3ba862e9d572ea0`;
macOSarm64 `920df4d17e78b3b0175af144eb318b0c74d1cf7b1d1251b326966f0e81977260`.
The58,694byte0.22.1 pure-Python wheel is SHA256
`cca895342e308174341b2cbf99a56bef291fbc0ef7b9e5412a0f26d653ba7094`, isolated
under ROOT/python, with no dependencies/global package writes. Versions/licenses
are recorded under the supplied root. No full-conformance/fuzz/pcap claim.
