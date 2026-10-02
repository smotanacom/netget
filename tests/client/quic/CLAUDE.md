# Raw QUIC client evidence

`real_server_test.rs` uses independent aioquic 1.3.0, with no netget server. Tests
exercise certificate/hostname/ALPN validation, binary and concurrent streams,
injected actions, script follow-ups, disconnect, local UDP ownership/release,
oversized responses, missing FIN and reset followed by a successful new stream.
Missing aioquic is a hard failure, not a skipped test. All traffic is loopback.

The Python helper in `tests/helpers/quic_peer.py` calls aioquic's own TLS, QUIC,
HTTP/3 and QPACK APIs. It supplies test behavior, not another netget wire codec.
Provenance: https://github.com/aiortc/aioquic/tree/1.3.0 (BSD-3-Clause).

Small isolated setup, with no pip cache or source builds:

```sh
python3 -m venv /tmp/netget-aioquic-peer
/tmp/netget-aioquic-peer/bin/pip install --no-cache-dir --only-binary=:all: aioquic==1.3.0
export NETGET_AIOQUIC_PYTHON=/tmp/netget-aioquic-peer/bin/python
./cargo-isolated.sh test --offline --no-default-features --features quic --test client --test server -- quic:: --test-threads=4
```

During the expansion use `python3 /private/tmp/netget-protocol-expansion-20261001/run_cargo.py`
in place of `./cargo-isolated.sh`. The existing programme-owned peer environment
is `/private/tmp/netget-protocol-expansion-20261001/peers/aioquic-env` (about 34 MiB).
No Docker image, Go build cache, new dependency version or extra target is used.
Shared registry/action/startup-default/pairing ratchets are run by the coordinator.

Final combined four-worker run passed all 14 tests (4 client, 10 server), none
ignored. Minimal `--features quic --lib` compilation and formatting also passed.
See server test notes for one unreproduced earlier mocked-test timeout.
