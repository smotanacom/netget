# StatsD collector validation

All tests are registered under `tests/server/mod.rs` and run with feature `statsd`.
No ignored tests or absent-peer skip paths. Reference peers must be installed explicitly.

- `codec_test`: published wire vectors; typed records; all six metric types; signed gauge
  preservation; sampling/tags; Unicode byte lengths, pipes and escaped newlines; invalid UTF-8,
  malformed/duplicate/unknown options, numeric non-finites, injection, dialect restrictions;
  exact byte and record limits on both parse and encode.
- `e2e_test`: real UDP sockets through `ServerForm`, collection without model calls, static/
  script handlers, oversize-prefix rejection, next-packet recovery, silence, no per-packet peer
  accumulation, actual socket release on stop, NetGet server/client pairing, and one mocked
  model call for an explicitly opted-in multi-record batch. Handwritten packets are wire tests, not an
  independent protocol implementation.
- `real_client_test`: independent Python `statsd==4.0.1` and Datadog `datadog==0.52.0` emitters.
  Assert parsed structured records for counters, gauge delta, timer, set, histogram,
  distribution, tagged metrics, Unicode/multiline event, multiline service-check message.
  Missing imports/process failures fail the test. Neither shares the codec with NetGet.

Bootstrap peers into a task-owned directory (small: Python dependencies ~6 MiB, StatsD Node
source <1 MiB). This requires network access and keeps TLS verification enabled. Node reference
collector package is `statsd@0.9.0`; its tarball is verified against npm SHA512 integrity before
safe extraction. Only its builtin UDP/stdout/custom-backend path is used, so optional native/
Windows/proxy dependencies and npm install scripts are unnecessary.

```sh
python3 tests/server/statsd/install_peers.py /tmp/netget-statsd-peers
export PYTHONPATH=/tmp/netget-statsd-peers/statsd-python
export NODE_PATH=/tmp/netget-statsd-peers/statsd-node/node_modules
./cargo-isolated.sh test --no-default-features --features statsd --test server --test client -- statsd:: --test-threads=4
```

This programme uses its shared serialized `run_cargo.py` wrapper instead of `cargo-isolated.sh`
to maintain a free-space reserve. Tests add no Rust dependencies. Protocol remains Experimental:
no pcap oracle, fuzz execution or complete DogStatsD conformance is claimed. External collector
coverage is classic StatsD, not Datadog Agent. See the client test notes for that distinction.
