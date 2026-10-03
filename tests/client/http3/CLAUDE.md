# HTTP/3 client tests

All tests run on loopback without model calls. Missing aioquic is a hard failure.
`e2e_test.rs` drives an independent aioquic 1.3.0 server with authenticated TLS,
GET/POST semantic bodies/default and overridden headers, urgency, trailers,
concurrent streams, incoming/outgoing size limits, absolute-target rejection,
certificate trust/name failures, timeout/reuse and responsive disconnect/removal.
The manual-handler saturation test proves every accepted response is parked,
the 32-slot total cap refuses further work, and disconnect clears all handlers.
`command_channel_test.rs` uses a real authenticated peer and verifies unknown
injected actions, logs, disconnect status and command-handle cleanup.
The old ignored tests against a nonexistent server have been replaced.

The separate `tests/http3_cancellation_test.rs` deliberately polls a real
h3-quinn adapter read to Pending before stopping it and reading its stream ID.
Upstream 0.0.10 panics at src/lib.rs:394. The vendored patch keeps the Quinn
receive stream in place; read_chunk is cancellation-safe. Real client timeout and
removal tests exercise the same path through HTTP/3 request streams.

Bootstrap in the persistent programme peer directory (no global install/cache):

```
python3 -m venv /Users/matus/dev/netget/.protocol-expansion-20261001/peers/aioquic-env
/Users/matus/dev/netget/.protocol-expansion-20261001/peers/aioquic-env/bin/python -m pip install --no-cache-dir -r tests/helpers/aioquic-requirements.txt
export NETGET_AIOQUIC_PYTHON=/Users/matus/dev/netget/.protocol-expansion-20261001/peers/aioquic-env/bin/python
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features http3 --test client -- http3 --test-threads=4
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features http3 --test http3_cancellation_test
```

The reproducible peer pins are in tests/helpers/aioquic-requirements.txt.
Only aioquic is required by the helper API; the exact peer environment used for
this pass also pins pylsqpack 0.3.24, cryptography 50.0.2, pyopenssl 26.4.0,
service-identity 26.1.0, certifi 2026.7.22, attrs 26.1.0, cffi 2.1.1 and
pycparser 3.0. Normal repository work can use ./cargo-isolated.sh. Restricted
sandboxes must grant loopback socket permission. Experimental evidence only;
transport framing in h3/quinn is shared by the two NetGet roles.
