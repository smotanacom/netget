# Generic gRPC Client Verification

`streaming_test.rs` runs against mandatory independently generated grpcio 1.75.1 C++ peers
and a NetGet server. Missing Python packages/protoc fail; no ignore/skip gate. Pins and
setup are in `tests/helpers/grpcio-peer-requirements.txt`; set NETGET_GRPCIO_PYTHON to the
owned venv. Peer readiness reports its actual port0 bind; Rust owns each child. TLS fixtures
use openssl and ordinary certificate verification.

Checks cover reflection schema discovery (independent v1alpha fallback and NetGet v1),
all three stream shapes/gzip/repeated/maps, typed request input-ready backpressure,
Collect/Chat half-close, cancellation/reused-ID rejection, malformed metadata/type/method
refusals followed by recovery,16 parked-handler admission with responsive controls,
whole RPC deadline, idle disconnect and removal intercept cleanup. Exact4 MiB/+1 probes
exercise plain/gzip requests and responses. Verified custom-CA TLS succeeds; wrong hostname
and untrusted certificates fail. NetGet pair covers both native roles.

Final client suite: 14 passed, 0 failed/ignored at 100 threads, 4.80s (includes four retained
unary checks). Exact 1 MiB CA acceptance and oversized/directory/FIFO rejection are included.
The initial disk-guard refusal executed zero checks. Preparatory logs retain the real TLS
provider ambiguity and the correctly rejected CA:TRUE end-entity fixture; both were fixed.
Logs: programme logs/item68-grpc-client-*.log.

Legacy `command_channel_test.rs` retains two in-process unary checks: honest Sent9 for
Calculator/Add (5-byte prefix+4 protobuf bytes), repeated/map fields and wrong-type refusal
followed by a usable connection. `e2e_test.rs` retains mocked subprocess unary success and
connection error. These make no claim about independent peer interoperability.

Use the shared run_cargo.py guard with test --locked --offline --no-default-features
--features grpc --test client grpc:: -- --test-threads=100 --nocapture. No real model inference
is required for the new cases: static, Python script or manual rules answer actual wire events.
