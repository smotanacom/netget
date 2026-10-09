# InfluxDB v2 collector tests

Native fixtures prove typed float/int64/uint64/bool/string values,
name/string escaping, precision/range normalization, syntax partial errors and all bounds.
Loopback tests exercise configured handlers, default model suppression, token privacy,
gzip limits, failure responses, mixed action failure, task cancellation and socket release.
The mandatory official Python InfluxDB2 client 1.50.0 emits actual typed writes and validates
JSON rejection/Retry-After; dependencies pinned in install_peers.py. MIT licenses verified.
missing peers must fail, with no skip gates.

The client receiver evidence includes a mandatory MIT official line-protocol v2.2.1 decoder-backed
v2 HTTP adapter and the actual official InfluxDB 2.9.1 service with Python readback. The
decoder adapter is labeled precisely and checks route/query/auth/content-type/encoding.
The real service test is mandatory on Linux/macOS (no skip-if-missing); Windows has native
and decoder-backed evidence only. Linux daemon bootstrap supports amd64/arm64; macOS
requires existing amd64 execution (native or Rosetta); installation is never performed.

```sh
python3 tests/server/influxdb/install_peers.py /tmp/netget-influx-peers
python3 tests/client/influxdb/install_daemon.py /tmp/netget-influx-peers
export PYTHONPATH=/tmp/netget-influx-peers/python
export NETGET_INFLUX_PYTHON=python3
export NETGET_INFLUX_RECEIVER=/tmp/netget-influx-peers/influx-decoder-receiver
export NETGET_INFLUX_DAEMON=/tmp/netget-influx-peers/influxd-2.9.1
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features influxdb --test server --test client -- influxdb:: --test-threads=4
```

The programme wrapper is the required local Cargo entrypoint. CI uses the same feature/test
selection with ordinary Cargo under its runner limits. Each daemon test owns a temporary
data directory, process and log, disables telemetry and checks 2.9.1 before setup; process
drop kills it on any failure. Python setup tokens pass by stdin, never command arguments.
Header and body fake-clock tests use separate Tokio runtimes because Hyper's header timer
starts from std::Instant. Parked handlers outlive wire deadlines and remain stoppable.

Local validation on 2026-10-02: 18 server + 10 client = 28 PASS, 0 failed/ignored, including
official Python 1.50.0 and official InfluxDB 2.9.1 via existing macOS amd64 support. Standalone
`cargo check --no-default-features --features influxdb --tests` PASS; CI Linux service
execution remains a separate platform check.
