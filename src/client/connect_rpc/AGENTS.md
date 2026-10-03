# Native binary Connect RPC client

`connect_rpc` registers `connect-rpc` / ConnectRPC as an Experimental native cleartext
HTTP/1.1 protobuf unary/server-streaming client. **JSON messages, GET, client/bidirectional
streaming, reflection, WebSocket and TLS are excluded.** Unsupported startup selections
are rejected by the declared parameter contract; no silent encoding/TLS fallback occurs.
No actual browser, CORS, pcap, fuzz or universal conformance claim is made.

The client shares the gRPC-Web HTTP/1 owner, not the Web wire format. A binding selector
chooses Connect request/response framing, action names and typed event definitions. Native
gRPC keeps its transport. The same registered AppState task polls the HTTP driver, one RPC,
16 handlers, injected commands and bounded event queues; no driver/model task is detached.
A socket guard closes both directions on drop. Cancel disconnects the whole session and
removes pending manual intercepts; no retries/reconnect or application domain store exists.

connect_rpc_call takes a fresh positive u32 call_id, service, method and field-name request,
optional ASCII metadata and gzip. At most 256 IDs can be used, with no reuse. Local schema,
typed values and metadata are validated before wire work; reachable protobuf bytes fields
are rejected. The audited native DynamicCodec/value and immutable schema loader are reused.
Startup proto_schema is required. connect_timeout_secs defaults10 (1..60), including schema,
TCP and HTTP; rpc_timeout_secs defaults300 (1..3600), including model backpressure;
idle_timeout_secs defaults120 (1..3600) applies when RPCs, handlers and queued events are idle.

connected/opened/message/ended events expose schema service names, call IDs, typed responses,
counts, final numeric status and bounded diagnostics. OPENED and ENDED carry bounded ASCII
leading/trailing metadata arrays. Unary trailer-prefixed headers are stripped transparently;
stream EndStream metadata becomes native status/trailers internally. No binary metadata or
binary error detail blob is exposed. Calls whose response fails before validated HTTP EOF
close the owner rather than trying to drain/reuse it; legal Connection:close still preserves
already decoded messages and the final result before teardown. Automated follow-ups stop
at depth4; every handler returns at most16 actions, with 16 queued events/16 active handlers.

Unary wire bodies are bare protobuf; errors are Connect JSON with meaningful HTTP status.
Absent/malformed errors use the specified HTTP fallback mapping. Streams use five-byte
message envelopes and require one complete final bit-1 EndStream JSON object followed by
actual EOF. Streaming error:null, invalid/duplicate code fields, duplicate metadata keys, trailing
garbage, reserved flags and HTTP trailers are refused. Gzip must be negotiated; CRC and
all members are checked, including compressed EndStream. Advertised lengths are checked
before payload allocation. Both encoded/expanded messages cap at4 MiB; EndStream/errors
at16 KiB; responses at256 messages; HTTP at64 fields/32768 aggregate bytes. Metadata caps
at16 keys/32 values/8 KiB, names128/values1024. Reserved and binary outgoing fields refuse.

Mandatory exact Connect-ES2.2.0/protobuf-es2.16.0 peers test both roles and NetGet pairs.
Node and Fetch are different transports in one library family; they are not two independent
libraries or proof of browser execution. See tests/client/connect_rpc/AGENTS.md.
Primary specification: https://connectrpc.com/docs/protocol/.
