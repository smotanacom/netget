# Graphite collector validation

`codec_test`: UTF-8 paths/tags, fractional timestamps and receiver-time sentinel, every stream
fragment boundary, coalesced batches, malformed/nonfinite fields, injection, byte/count limits,
bounded pending bytes and incomplete EOF. `e2e_test`: real fragmented/coalesced TCP streams,
typed observation, explicit static/script handlers, no default model calls, offending-peer
isolation, manual-handler cancellation, established-socket and listener release, absolute
deadline despite trickled bytes, explicit model opt-in, and NetGet server/client pairing.

`real_client_test` uses independent `graphyte==1.7.1` to emit values, UTF-8 paths and tagged
series. It requires received structured events, not just a successful send. The independent
receiver in the client suite is official `carbon==1.1.10`, detailed in its test notes.
Missing packages/process failures fail tests; there are no ignored or absent-peer skips.

Carbon 1.1.10 uses Python APIs removed in 3.12, so the pinned peer environment uses Python
3.11. Bootstrap (~44MiB including Twisted) is isolated to the supplied directory; pip cache
is disabled and TLS verification remains enabled. No database/writer service is started.

```sh
python3.11 tests/server/graphite/install_peers.py /tmp/netget-graphite-peers
export PYTHONPATH=/tmp/netget-graphite-peers/graphite-python:/tmp/netget-graphite-peers/graphite-python/opt/graphite/lib
export NETGET_GRAPHITE_PYTHON=python3.11
./cargo-isolated.sh test --no-default-features --features graphite --test server --test client -- graphite:: --test-threads=4
```

This programme uses the serialized shared `run_cargo.py` wrapper instead of `cargo-isolated.sh`
to reserve free disk space. Source fixtures are wire tests, distinguished from independent
peer tests. No fuzz execution, pcap oracle or complete Graphite platform conformance claimed.
