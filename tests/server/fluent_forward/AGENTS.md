# Forward collector tests

Native fixtures cover all four carriers, integer/EventTime timestamps, typed/nested records,
all stream split boundaries, coalescing, nil heartbeat, legacy str PackedForward, exact bytes,
malformed/unknown markers, shared packed value budgets, declared length/count/depth bombs, tag/entry/time/message/buffer
bounds, incomplete EOF, compressed bombs/trailers and size mismatch. Network tests cover
correlated ACKs, default model suppression, static/script/manual/model routes, memory,
rejection and mixed failed action/dispatch-error decision logs before ACK, absolute read deadline,
256-peer connection cap and slot return on malformed input, stop and both-role ACK follow-ups.
Independent fluent-logger0.11.1 emits real Message mode with integer/EventTime timestamps.

Pinned peers bootstrap into the supplied root with no global installs. Python>=3.8 and pip;
Ruby>=3.2 and gem; compiler/make/Ruby headers for native gems. Official Fluentd1.19.4 is pinned,
transitive dependency versions are recorded under root/ruby-dependencies.txt. Tested Ruby4.0.6
and Python3.14. The peer environment measured 35MiB here. No storage service or heavy container.

```sh
python3 tests/server/fluent_forward/install_peers.py /tmp/netget-forward-peers
export PYTHONPATH=/tmp/netget-forward-peers/python
export NETGET_FORWARD_PYTHON=python3
export NETGET_FORWARD_RUBY=ruby
export GEM_HOME=/tmp/netget-forward-peers/ruby
export GEM_PATH=/tmp/netget-forward-peers/ruby
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features fluent-forward --test server --test client -- fluent_forward:: --test-threads=4
```

Use the programme's serialized shared target wrapper for all Cargo commands. Missing peers
fail tests. Native fixtures distinguish codec coverage from independent domain peer evidence.
No ignored/skipped tests, pcap oracle, fuzz execution or complete-platform conformance claim.

Validated 2 October 2026 with the command above, `--features fluent-forward` alone:
17 server tests and 7 client tests passed, 0 failed/ignored. The server fixtures also check
outbound packed/compressed inner depth limits, preventing encoder/decoder disagreement.
Standalone `check --no-default-features --features fluent-forward --tests` and
`clippy --no-default-features --features fluent-forward --lib --test server --test client --
-D clippy::correctness -D clippy::suspicious` passed through the same shared wrapper.

Peers are test-only: Fluentd and fluent-logger are Apache-2.0, msgpack is Apache-2.0.
License evidence was checked in the pinned installed packages' `LICENSE`/`COPYING` files;
the msgpack source license is https://github.com/msgpack/msgpack-python/blob/v1.1.2/COPYING.
The Ruby harness uses unmodified official in_forward framing/EventTime/gzip parsing and ACK
handling; its output plugin observes typed records only. Both test peers enforce pinned versions.
