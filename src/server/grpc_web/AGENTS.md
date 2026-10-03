# Binary gRPC-Web server

`grpc-web` is a separately registered, native `Experimental` binding for binary
protobuf unary and server-streaming RPCs over cleartext HTTP/1.1. **Text/base64
gRPC-Web is rejected with HTTP 415.** JSON, native gRPC content types, client and
bidirectional streaming, reflection and WebSocket transport are outside this
selected scope. Receiver TLS, browser execution, pcap and fuzz evidence are unproved.

## Ownership and libraries

`tonic-web` **0.12.3**, MIT, is an unmodified server response-framing dependency.
Use `GrpcWebLayer` directly. Its convenience `enable()` adds mirrored credentialed
CORS and is deliberately not used. `tonic` 0.12.3 owns protobuf length prefixes,
message gzip and status semantics. The existing local tonic patch caps decompressed
messages before allocation; preserve its version, license and provenance.

The shared native gRPC service supplies immutable descriptors, `DynamicCodec`, typed
value conversion and stream controls. `WebCore` admits a known unary/server-streaming
method before reading its request and retains the same RPC permit and whole deadline
through response EOS. The request buffer reads at most 4 MiB + 6 bytes (one framed
message bound plus one probe byte), and waits for EOF only within that bound and
deadline. Legal exact/+1 inputs receive deterministic status; arbitrary longer inputs
close promptly without draining their declared remainder. HTTP trailers in requests
are refused. Exactly one message and EOF are required before handler work.

The accept task and each TCP owner are registered with `AppState`. The connection's
`OwnedExecutor` tracks its HTTP driver, peer command worker and hard deadline timers.
Connection teardown aborts all children and removes peer handles, connection state and
manual intercepts. An expired Web deadline remains armed after an early timeout
response so Hyper cannot keep draining an unfinished request indefinitely. Native
HTTP/2 deadline guard behavior is unchanged.

## Handler surface

Both supported method shapes raise `grpc_stream_opened`; requested waits raise
`grpc_stream_tick`. Events contain field-name JSON, descriptors' response hints,
stream ID, method shape and counts. Unary replies also use `grpc_stream_send` followed
by `grpc_stream_finish`; `grpc_unary_response` is not advertised or accepted here.
`grpc_stream_cancel`, `grpc_stream_wait` and `grpc_error` retain the shared controls.
An unanswered request fails closed with status 13. Application failures use HTTP 200
and a final Web trailer frame, not an invented successful empty protobuf reply.

There is no domain store. The schema is immutable and per-connection registries retain
only bounded wire lifecycle controls. Reachable protobuf `bytes` fields, including
nested/map values, are rejected; no encoded message or descriptor blob enters events.

`proto_schema` is required: prefer inline proto text; `.proto`/`.pb` paths and existing
precompiled descriptor sets are also supported through `grpc::schema`. Shared schema
limits are 4 MiB, 128 files/services, qualified-name depth 32 and bounded regular-file
reads/protoc output. Protoc has a 10-second deadline, 16 KiB stderr budget and kill-on-drop.
The shared value codec caps depth 32, 100000 nodes and retained content at 8 MiB.

## Bounds and CORS

| Bound | Behavior |
|---|---|
| 4 MiB encoded/decoded protobuf message | Checked independently before wire allocation and after gzip; status 8 on refusal |
| 4 MiB + 5 framed request, plus one probe byte | Early buffer bound; larger/trickled uploads are not drained |
| 256 TCP peers | 257th receives HTTP 503, Retry-After 5 and close |
| 64 RPCs across all peers | Further RPC receives status 14 before buffering/model work |
| 64 HTTP headers / 32768-byte Hyper buffer | HTTP 431 before handler work |
| 2048-byte path; no query | HTTP 400 |
| 30-second first-byte / 120-second idle | Close silent peers; an admitted RPC holds activity through body EOS |
| Whole RPC: 300 seconds by default, 1..3600 configurable | Includes upload, handlers, response backpressure; unique grpc-timeout may only shorten it |
| 256 output messages, 16 pending controls, 4 MiB pending bytes | Shared stream controller; finish requires one output for unary |

`allow_origin` is optional and names one exact HTTP(S) origin without a trailing slash.
Omitting it rejects all requests carrying Origin. CORS never advertises wildcard origins
or credentials. A valid OPTIONS preflight permits POST and the documented Web/gRPC
request headers only; values, duplicate fields and header count are bounded. CORS is
not authentication.

## Evidence

`tests/server/grpc_web` requires pinned Connect-ES 2.2.0/protobuf-es 2.16.0. Node and
Fetch transport tests are distinct transport implementations in the same Connect
family, not two independent libraries or evidence of browser execution. Actual wire
tests cover both method shapes, gzip, status messages containing colons, exact/+1
requests, rejection, CORS, cancellation, cap exhaustion and connection deadlines.
Read its AGENTS.md for installation and retained failure evidence. Native gRPC suites
are the regression neighbors for the shared service seam.

Primary protocol reference: [gRPC-Web protocol](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-WEB.md).
