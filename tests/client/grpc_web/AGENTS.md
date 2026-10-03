# gRPC-Web client integration tests

Run the `client` target filtered by `grpc_web` with native `tcp,grpc-web` features and
the mandatory pinned Node/protoc setup described in `tests/server/grpc_web/AGENTS.md`.
`NETGET_GRPCWEB_NODE_DIR` points at the owned directory containing the four pinned
packages. Peer children bind their actual port zero and are killed on drop. Missing
peers are errors; there are no skip gates or ignored tests. The model endpoint is
intentionally unreachable: deterministic/manual/Python rules answer these tests.

```sh
python3 ../run_cargo.py test --locked --offline --no-default-features --features tcp,grpc-web --test client grpc_web -- --test-threads=100
```

The mandatory independent Connect-ES server exercises unary and server-streaming calls,
gzip, nested/repeated/map values, reusable HTTP/1 sessions and status 7 with a colon in
its message. NetGet pairs assert all three response events reach access logs before a
final-status disconnect. Exact 4 MiB and +1 decoded compressed responses and the 257th
message test both status 8 and teardown; local oversized requests fail before ID use.

Admission tests reject zero/reused IDs, unknown/excluded methods, type mismatches,
reserved/binary metadata, metadata field/length/aggregate overflow and the 257th consumed
call ID. The exact 8 KiB metadata boundary and all 256 legal IDs complete actual RPCs.
Automatic script-driven followups complete five calls (initial plus depth 4), then stop.
Seven raw HTTP response cases include a valid data/trailer reply with Connection: close,
then missing/truncated/invalid trailers, unsupported flags,
oversized advertised message length without allocating its payload and duplicate initial
status; each must record its terminal diagnostic before removing the client handle.
The valid closing response must also preserve its decoded typed message.

Manual rules fill all 16 handler slots; injected cancellation remains responsive and
clears every intercept while disconnecting the session. Wrong-ID cancellation and a
second active RPC fail. RPC and idle deadlines are exercised at one second.

Initial peer gzip responses exposed legal 0x81 compressed trailer frames; the bounded
adapter now validates negotiated gzip, encoded/expanded trailer limits, CRC and final EOF.
An oversized decoded response initially vanished without a terminal diagnostic when
the HTTP session was dropped too early; the owned operation now records its status before
teardown, including a transport EOF racing completion. Initial failures remain in durable
programme logs. This suite proves cleartext native wire behavior, not browser/TLS/pcap/fuzz
or text-mode support.

Final macOS validation: all 9 Web client tests and 14 native gRPC client neighbors
passed together at 100 test threads in 5.03 seconds, including the legal closing-response
case. No tests failed or were ignored in that final run. The preceding small event-field
borrow correction failed compilation with two E0505 errors and executed no tests;
the corrected run and earlier successful suites remain in the logs.
