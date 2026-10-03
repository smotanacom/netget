# Native binary Connect RPC server

`connect_rpc` registers `connect-rpc` / ConnectRPC as an Experimental native cleartext
HTTP/1.1 binding for protobuf unary and server-streaming methods. **JSON message codecs,
unary GET, client/bidirectional streaming, reflection, WebSocket, TLS and Origin requests
are refused.** Browser/CORS execution, pcap, fuzz and universal conformance are unproved.

Unary requests/responses contain bare protobuf, with Connect JSON error objects and the
specified non-200 HTTP status mapping. Streams use five-byte envelopes and a mandatory
final flag 2 JSON EndStream. No HTTP trailers are emitted. Gzip uses unary HTTP encoding
or per-message stream flags, and only negotiated gzip can compress a frame. Errors and
EndStream always use JSON even though protobuf message JSON is outside this subset.

The existing tonic0.12/prost0.13 DynamicCodec, immutable schema loader and audited value
converter/controller are reused. No new dependency package or application domain store
is added. The current official Rust Connect runtime uses a different protobuf stack;
this boundary keeps the existing dynamic descriptors. Reachable bytes fields are rejected
before field-name values enter a handler. Schema limits remain 4 MiB/128 files/services,
name depth32 and bounded regular-file/protoc work; values have depth32/100000 nodes/8 MiB.

Both method shapes raise grpc_stream_opened and grpc_stream_tick as appropriate. Use
shared grpc_stream_send/finish/cancel/wait/grpc_error controls. Unary requires one output
and finish. An unanswered handler fails closed with INTERNAL. connect_rpc_metadata
replaces bounded ASCII headers or trailers: leading metadata must precede the first
response message, and later leading changes yield FAILED_PRECONDITION. Trailers precede
finish; they become trailer-prefixed unary HTTP headers or final EndStream metadata.
Events contain bounded ASCII request metadata arrays; no encoded protobuf/schema blob
or binary header/error detail enters model data. Binary request headers are rejected.

The accept task and TCP owners are registered with AppState. The owned executor contains
the HTTP driver, command worker and hard RPC timers. Admission precedes bounded buffering
and retains the same global permit/activity/deadline through final body EOF, error or drop.
Later HTTP/TCP buffering belongs to the connection owner/idle bound. An already-expired
timer stays armed when Hyper drains an unfinished rejected request. Stop/drop cancels
children and removes peer handles, connection state and manual intercepts.

| Bound | Behavior |
|---|---|
| 4 MiB encoded/expanded message | Check before allocation and after gzip; RESOURCE_EXHAUSTED |
| Bare/framed request cap plus one probe byte | Await EOF only within the cap/deadline; larger uploads close without unlimited drain |
| 16 KiB encoded/expanded EndStream/error JSON | Bounded before parsing/forwarding |
| 16 metadata keys / 32 values / 8 KiB | 128-byte names, 1024-byte ASCII values; reserved/binary fields excluded |
| 256 TCP peers / 64 global RPCs | Refuse extra peers/RPCs before model work |
| 64 HTTP fields / 32768 aggregate bytes | HTTP431 before semantic parsing/model work |
| 2048-byte path; no query | HTTP400 |
| 30-second first byte / 120-second idle | Actual connection lifetime bounds |
| Whole RPC 300 seconds, configurable 1..3600 | Includes upload, model and body backpressure; unique positive 1..10-digit connect-timeout-ms may only shorten it |
| 256 output messages / 16 queued controls / 4 MiB pending output | Shared bounded stream controller |

The server requires unique connect-protocol-version:1. Unsupported compression reports
UNIMPLEMENTED with supported encodings; identity is always accepted. There is no dedicated
Connect port: this HTTP carrier uses the port the caller chooses. No authentication or
TLS verification claim is made for this cleartext subset.

Mandatory pinned Connect-ES2.2.0/protobuf-es2.16.0 peers test both roles and NetGet pairs.
Node and Fetch are transports in one library family, not two independent libraries.
See tests/server/connect_rpc/AGENTS.md for exact peer setup, checks and retained failures.
Primary specification: https://connectrpc.com/docs/protocol/.
