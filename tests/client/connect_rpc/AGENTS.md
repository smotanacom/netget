# Connect RPC client tests

Mandatory exact Connect-ES 2.2.0/protobuf-es 2.16.0 peers require Node >=22 and protoc.
Set NETGET_CONNECT_RPC_NODE_DIR to the peer directory installed from the existing
pinned tests/helpers/grpcweb-package.json and grpcweb-package-lock.json. Peer processes
bind port zero and report their actual port; missing dependencies fail, never skip.
The selected cleartext HTTP/1.1 scope is binary protobuf unary/server streaming.
JSON message codecs, GET, client/bidi streaming, reflection, TLS and browsers are excluded.
All programme Cargo commands use ../run_cargo.py and the shared serialized disk guard.

All nine client tests passed at100 threads on 3 October 2026, in a32-check combined run
with native gRPC14 and gRPC-Web9. The mandatory independent Connect-ES HTTP/1 server
exercises bare unary, server streaming, gzip, repeated/map fields, ASCII leading/trailing
metadata and colon-containing status messages. A NetGet pair checks ordered typed
messages before final status and legal peer disconnect. Node and Fetch are transports
in one library family, not two independent implementations or a browser proof.

The other checks exercise method/schema/value rejection before wire work, reserved and
binary outgoing metadata, fresh/non-reused IDs, exact4 MiB/+1 decoded and expanded gzip
responses,256/+1 response counts, local oversized requests, cancellation with16 parked
handlers, whole RPC/idle/stop cleanup, malformed HTTP/EndStream/compression responses,
automatic follow-up depth4, metadata limits and the actual256-ID budget. Failures before
validated HTTP EOF close the owned session; no background unbounded drain is performed.

The durable client result is logs/item13-connect-client-neighbors-final.log in the
programme directory. Connect wire8, Web wire8, audited value7 and pinned tonic3 CPU
checks passed in item13-connect-wire-neighbors-ready.log. Isolated Connect and Web
library/binary feature builds, all-target correctness/suspicious/unused_must_use lint
and whole-repository formatting passed. Missing peers fail, no tests are ignored, and
JSON message codecs, GET, client/bidi streaming, reflection, TLS, browser/CORS, pcap and
fuzz evidence remain outside this Experimental scope.
