# Binary gRPC-Web client

This separately registered `grpc-web` client is `Experimental`: cleartext HTTP/1.1,
binary protobuf unary and server-streaming calls. **Text/base64 mode is unsupported
and binary content types are enforced.** Client/bidirectional streaming, reflection,
WebSocket, TLS, automatic retries and reconnect are excluded. Pcap, fuzz and browser
execution evidence are unproved. Startup rejects undeclared transport options.

## Typed calls and connection ownership

`proto_schema` is required and uses the bounded shared gRPC schema loader. Prefer inline
proto text. The schema is immutable. Call with `grpc_web_call`, a fresh positive u32
`call_id`, fully qualified service, method and field-name JSON request. Reachable `bytes`
fields are rejected in both descriptors. Existing `DynamicCodec`/value conversion
validate type/range, depth 32, 100000 nodes and retained 8 MiB; protobuf messages are
capped at 4 MiB encoded and decoded. Descriptors and wire blobs never enter model events.

Only one RPC is active per HTTP/1.1 session. At most 256 fresh IDs are consumed, with
no reuse; validation failure before admission leaves the ID available. Metadata has
at most 16 ASCII fields, 128-byte lowercase names, 1024-byte values and 8 KiB aggregate
name/value content. Reserved HTTP/gRPC and binary metadata are rejected. Optional
`gzip: true` compresses the request; gzip responses are accepted.

`grpc_web_connected` exposes schema service names and `tls_verified: false`.
`grpc_web_opened`, `grpc_web_message` and `grpc_web_ended` expose call ID, typed values,
sequence/count and bounded final status. Successful final status requires a validated
trailer and HTTP body EOF. Response events enter the handler queue before the terminal
event; up to 16 handlers may run concurrently, so their completion order is not promised.

The one registered client owner polls Hyper's driver, RPC futures, event handlers and
injected commands. There is no detached Hyper worker or background response drain.
There are 16 active handlers, 16 queued events and a 16-event transport channel; handlers
may return at most 16 actions. Automatic RPC followups stop at depth 4. Injected commands
remain responsive while manual handlers are parked.

`grpc_web_cancel` must name the active call. It records CANCELLED and disconnects the
HTTP/1.1 session. Disconnect, removal, deadline or incomplete/refused response drops
the RPC, handlers, driver and socket together, clearing manual intercepts and the
client handle. Local refusal/transport failure is recorded as `grpc_web_ended` before
teardown; queued decoded values are recorded too, without starting new handlers during
teardown. A fully validated application status, including a nonzero status, permits
another call while the connection remains open.

## Response framing

The unmodified tonic-web server dependency is not used as a client adapter. Its adapter
buffers advertised messages before tonic can limit them, accepts incomplete trailers
and mishandles colons in trailer values. `server::grpc_web::wire` is the bounded seam
before tonic decoding: read the 5-byte prefix, validate flag/length, then allocate only
the admitted frame. Tonic still owns message decoding and bounded gzip expansion.

Flags 0/1 are data and 0x80/0x81 are final uncompressed/gzip trailer blocks. Compressed
trailers require negotiated gzip. Encoded and expanded trailers are each at most 16 KiB,
32 fields, 128-byte lowercase names and 4096-byte visible ASCII values. Parsing splits
only the first colon and accepts the protocol's optional final CRLF. Status must be
unique and in 0..16; duplicate reserved trailers, missing/truncated trailers, HTTP trailers,
unsupported flags, additional data after the trailer and bad gzip are refused. At most
256 response messages are admitted. A trailers-only status in initial HTTP headers is
accepted only with actual empty body EOF.

## Deadlines and evidence

Connect defaults to 10 seconds (1..60), including schema/TCP/HTTP setup. The whole RPC
defaults to 300 seconds (1..3600), including event/model backpressure. Idle defaults to
120 seconds (1..3600), and applies only with no RPC or handler work. Both HTTP directions
use Hyper's 64-header/32768-byte header buffer bounds.

Mandatory independent Connect-ES server tests cover typed nested/repeated/map values,
gzip, nonzero status with colon-containing messages, decoded exact/+1 limits and message
count. NetGet pairs, malformed response fixtures, ID/metadata budgets, automatic
followups, manual cancellation and deadline cleanup cover lifecycle behavior. See
`tests/client/grpc_web/AGENTS.md` for exact execution evidence and peer setup.
