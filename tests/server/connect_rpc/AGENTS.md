# Connect RPC server tests

Mandatory exact Connect-ES 2.2.0/protobuf-es 2.16.0 peers require Node >=22 and protoc.
Set NETGET_CONNECT_RPC_NODE_DIR to the peer directory installed from the existing
pinned tests/helpers/grpcweb-package.json and grpcweb-package-lock.json. Peer processes
bind port zero and report their actual port; missing dependencies fail, never skip.
The selected cleartext HTTP/1.1 scope is binary protobuf unary/server streaming.
JSON message codecs, GET, client/bidi streaming, reflection, TLS and browsers are excluded.
All programme Cargo commands use ../run_cargo.py and the shared serialized disk guard.

The 13 server checks passed on 3 October 2026. Twelve passed in a 100-thread combined
run with native gRPC and gRPC-Web; the corrected actual connection-deadline check passed
separately. The final successful set comprises 47 server checks: Connect13, native24,
Web10. The unchanged Web150-second connection fixture was excluded from this neighbor
run, not ignored or skipped within its suite. The 32 client neighbors and 26 CPU checks
also passed. Isolated tcp,connect_rpc and tcp,grpc-web library/binary builds, all-target
correctness/suspicious/unused_must_use lint and whole-repository formatting passed.

Coverage includes mandatory Node and Fetch transports from the same Connect-ES library
family, unary/server streaming, gzip, repeated/map fields, status and ASCII metadata;
leading-header sealing and trailing metadata on errors; unsupported media/method/version/
Origin forms; independent exact4 MiB/+1 plain and gzip requests; parked handlers,
unfinished uploads and unread responses; stop/manual cleanup; TCP256/257 and global
RPC64/65 recovery; oversized uploads without draining an advertised billion-byte body;
and actual64-field/32768-byte HTTP parser limits. No browser execution is inferred from
the Node Fetch transport, and no second independent library, TLS, pcap or fuzz proof is
claimed.

The empty-response lifetime probe sends 66 Content-Length:0 unary requests, each with an
empty protobuf response, and checks that the accepted remote address stays the same.
All replies succeeded; the wire used chunked HTTP with a final zero chunk. Successful
nonempty and empty unary connections closed at120.043296 and120.043850 seconds. The
first-byte30-second bound passed in the same check. watch_idle polls every idle/20, six
seconds here, so the fixture allows one poll interval plus three seconds of scheduling
slack. Admission remains held until final body explicit EOF/error/drop; no TCP-flush
ownership is claimed after EOF.

Eight tests in tests/connect_rpc_wire_test.rs cover arbitrary envelope splits, mandatory
unique/non-null final EndStream followed by actual EOF, reserved flags and HTTP trailers,
message/count and metadata exact/+1 limits, gzip CRC/all-member/expansion and compressed
EndStream16 KiB/+1 limits, ten-digit timeout uniqueness, all16 error HTTP mappings and
bare unary bodies with prefixed trailing metadata. Their final combined CPU run also
passed Web8, audited value7 and pinned tonic3 checks.

Retained programme logs live in .protocol-expansion-20261001/logs. The successful actual
deadline evidence is item13-connect-connection-deadlines-ready.log. The combined
item13-connect-server-neighbors-ready.log retains46 passes and the script-fixture
HTTP500/status-assertion failure; its exact failed source is preserved separately as
item13-connect-idle-second-fixture.rs. The original123-second wait timeout remains in
item13-connect-connection-deadlines-final.log. Neither failure is a successful run or
evidence of a changed runtime bound. Build-reserve refusals and the initial isolated
feature cfg error are retained under their original filenames. The corrected feature,
wire and lint logs end in -ready.log. No checks ran for the wrong-target argument error
or the disk-reserve refusals.
