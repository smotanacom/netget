# gRPC Client

Experimental dynamic protobuf client over tonic 0.12.3/prost 0.13/prost-reflect 0.14.
Unary behavior is retained; server-streaming, client-streaming and bidirectional methods
use tonic framing/gzip and typed field-name JSON. No protocol domain store is introduced.

Omit `proto_schema` to discover immutable descriptors through real reflection: v1 first,
v1alpha only after UNIMPLEMENTED. Reflection/schema startup has a whole connect deadline
(default 10s,1..60); descriptors are bounded to 128 files/services and 4 MiB. A changed duplicate
file is rejected. Connected events report service names; callers supply method and field
knowledge, rather than receiving a complete model-facing descriptor catalog. Binary
descriptors never enter model events. A supplied schema can be inline
proto text, `.proto`/`.pb` file, or legacy precompiled base64. Prefer inline text for models;
text/.proto needs protoc, reflection and precompiled descriptors do not. Both peers share
`server/grpc/schema.rs` and its bounded/cancellable compiler and symbol preflight.

`use_tls` defaults false for compatibility. True uses normal webpki roots, optional `ca_file`
(regular file≤1 MiB) and `server_name` verification. No skip-verifier exists. The connection
records real local/remote addresses. An owned socket shutdown guard prevents tonic's channel
worker from keeping a removed client connected; automatic reconnect is disabled.

## Streaming lifecycle

`grpc_stream_start` requires a fresh positive uint32 stream_id, service and streaming method,
with typed request required for server streaming and optional for the other forms. `gzip`
and bounded ASCII `metadata` are optional. `grpc_stream_send.message` queues a typed input;
`grpc_stream_finish` half-closes input while replies continue; `grpc_stream_cancel` aborts
that RPC. IDs are never reused and a session starts at most 256 streams.

`grpc_stream_opened` reports received response headers. For client streaming this can arrive
only after input EOF and its one response. `grpc_stream_message_received` carries typed
response fields and a sequence number. `grpc_stream_ended` reports the actual terminal gRPC
code and response count. `grpc_stream_input_ready` says the request encoder consumed the
queued item (input_sequence, queue_capacity 1); it permits a producer to queue its next input.
It makes no wire-delivery claim. Stream controls return Executed (queued), never Sent.

One registered client owner polls operations, event handlers and injected commands. It owns
all RPC/model futures; no per-stream task is detached. Controls remain responsive while
manual handlers park. There are 16 operation/pending slots,16 handlers,16 buffered events,
one queued input per stream,256 inputs/responses per RPC and 16 actions per handler. New-call
followups stop after depth 4; existing stream controls remain usable. Messages are≤4 MiB before/
after gzip, and reachable bytes fields/binary metadata are excluded from the new subset.
Metadata is≤16 fields, key 128/value 1024 bytes and aggregate 8 KiB; reserved transport/grpc headers
are rejected. Whole stream deadline defaults 300s (1..3600) including input/response/handler
backpressure; idle deadline defaults120s (1..3600), running only when no owned work remains.
Disconnect/removal drops all operations/handlers, closes the socket and clears intercepts.

## Preserved unary conversion and status evidence

**Cardinality is checked before the element type, and a bad value is refused.** Both were
wrong, and both failed silently:

- `json_to_proto_value` switched on `field.kind()`, which for a `repeated string` is
  `Kind::String`. `{"names": ["a","b"]}` therefore produced `Value::String("")`, and
  `DynamicMessage::set_field` **panics** on a cardinality mismatch — inside the client's
  spawned task, which swallows the panic, so `[ send ]` simply timed out. Repeated and map
  fields could not be sent at all. `json_to_field_value` now tests `is_map()` (a map field is
  also "repeated", of its synthetic entry message, so `is_list()` would take the wrong branch)
  then `is_list()` before falling through to the scalar path — the same shape
  `src/server/grpc/mod.rs` uses on its response path.
- Every scalar branch ended in `unwrap_or(0)` / `unwrap_or("")` / `unwrap_or_default()`, so a
  request the model got wrong was not refused: it went out carrying a **different value**.
  `{"a": "five"}` became `a = 0`, an out-of-range int was truncated with `as`, an unknown enum
  name became variant `0`, and invalid base64 became empty bytes. Every one of those is now an
  error naming the field, and integers are range-checked with `try_from`.

A field name the message does not declare is rejected. Non-object messages, multiple
members of one `oneof`, map keys that normalize to the same protobuf key, float overflow or
underflow to zero,
and non-finite protobuf values also return errors; conversion never substitutes an empty
message, zero, or null for those invalid inputs.

Both peers share `src/server/grpc/value_codec.rs`. It enforces depth 32 (root depth zero,
one step per field/map value/list element), 100,000 logical nodes, and an 8 MiB retained-content
budget. Accounting includes 64 bytes per node, 32 bytes per key plus its text, and string or
base64 content. These are conversion bounds rather than serialized wire-size limits. The
same budget crosses message/list/map recursion and siblings, and it is checked before
cloning strings, allocating decoded bytes, or emitting base64. Inputs constructed directly
in memory receive the same bounds as parsed inputs.

`tests/grpc_value_bounds_test.rs` uses in-memory descriptors and values to exercise both public
peer conversion paths, exact depth/node/byte boundaries, aggregate content, invalid values,
and nested protobuf round trips. It does not invoke protoc, model inference, or a network peer.

### Connection Management

- Single `Channel` created per client
- HTTP/2 connection pooling handled by tonic
- Connection tracked in client state with service metadata
- Persistent connection allows multiple RPC calls

### Error Handling

**gRPC Status Codes**:

- Success: Response converted to JSON, sent to LLM
- Error: Status code and message sent to LLM via `grpc_error` event
- LLM can decide how to handle errors (retry, log, etc.)

**`grpc-status` is read from the HTTP/2 trailers as well as the initial headers.** Reading only
the headers is why this client mishandled every real gRPC server: grpc-go and tonic put the
status in trailers on a normal unary call — headers carry it only in the "trailers-only" shape
— so a genuine `5 NOT_FOUND` arrived as "absent", which the client treated as success, and the
caller then got a meaningless "Response too short" from the empty body beside it.

**Both halves of that story were NetGet talking to itself.** The server had the mirror-image
defect — it put the status in the headers on a call that carried a body — so pointing this
client at that server showed nothing wrong in either direction. The server was fixed in
September 2026 and now emits real trailers (`src/server/grpc/AGENTS.md`), which is what a real
gRPC client refuses to proceed without. Reading both places is still correct here: an error
reply is legitimately Trailers-Only, and its status really is in the headers.

### The connection is claimed only once the request is built

`make_grpc_call` set the state machine to `Processing` first and reset it to `Idle` only after
the network call returned. Four `?`s sat in between — unknown method, a request the schema
refuses, an unbuildable HTTP request — and each returned early leaving the state `Processing`
**forever**: every later call answered "the client is already processing a call" and the client
was permanently wedged by one malformed request, with nothing in the log to say why. Everything
fallible now happens before the claim, and the claim itself is a single check-and-set under one
guard rather than two separate lock acquisitions.


## Unary command outcomes

`call_grpc_method` retains field validation, real Sent byte counts (5-byte prefix plus
protobuf payload), trailers/error parsing, one active unary-call state and bounded retry.
Its command reply precedes the response handler. The owner queues the response handler after
replying, so a parked manual event cannot hold the operator's send result. Unknown actions
are rejected; malformed requests never claim the unary state. `disconnect` closes the owner.

## Validation and limits

Mandatory generated grpcio 1.75.1 peers exercise independent reflection discovery, all stream
forms/gzip/repeated/maps, half-close, cancellation, message boundaries and verified TLS.
NetGet pairs exercise its own v1 discovery and both streaming roles. Client checks also cover
16 parked-handler admission, deadlines, idle/removal and refusal recovery. See
`tests/client/grpc/AGENTS.md` for commands, pins and exact results. Existing unary checks remain.
Receiver TLS, mTLS, streaming retries, load balancing, reflection authentication, fuzzing and
pcap evidence are not included in this Experimental scope.
