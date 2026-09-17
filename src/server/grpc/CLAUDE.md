# gRPC Server Implementation

A gRPC server whose service implementation *is* the handler. The protobuf schema is supplied at
startup; every unary RPC is decoded to JSON keyed by field name, handed to the handler, and the
handler's JSON is encoded back to protobuf.

**State**: `Experimental` · **Stack**: `ETH>IP>TCP>HTTP2>GRPC`

## Libraries

- **prost-reflect** 0.14 — `DescriptorPool` / `DynamicMessage`, so no code generation.
- **prost** 0.13, **prost-types** — `FileDescriptorSet` decode, message encode.
- **hyper** 1.x — `server::conn::http2` with a hand-written router. **tonic is not used** by
  this server despite being a dependency of the `grpc` feature; the routing, framing and
  status handling here are all local.
- **protoc** — required on `PATH` unless a pre-built `FileDescriptorSet` is supplied.

## Schema input

`startup_params.proto_schema`, tried in this order:

1. **base64 `FileDescriptorSet`** — no protoc needed. Note that a schema which happens to be
   valid base64 but is not a valid descriptor set is a hard error rather than falling through.
2. **path to a `.proto` or `.pb` file** — read from anywhere on disk, with the file's directory
   added as `--proto_path`, so `import` can pull in siblings.
3. **inline `.proto` text** — compiled with protoc.

**Do not tell a model to use base64.** An earlier version of this document called base64
"recommended"; models truncate long base64 strings inside JSON responses, and the startup
parameter description and the E2E test both say the opposite. Inline proto3 text is the form to
use. Reaching a schema by file path is also driven by model output and reads an arbitrary local
file, which is worth knowing before exposing `open_server` to an untrusted instruction.

`enable_reflection` used to be a startup parameter. It only ever changed a log line — see below.

## Reflection is not served

`grpcurl` with no `-proto`/`-protoset` begins with
`grpc.reflection.v1.ServerReflection/ServerReflectionInfo`. **There is no route for it**, so
that request is answered `12 UNIMPLEMENTED` and `grpcurl` cannot introspect this server.

This used to be worse than absent: a `tonic_reflection` service was built into a variable named
`_reflection_service`, dropped at the end of scope, and the server logged "gRPC reflection
enabled" while `metadata()` and this file both advertised it as a design principle. The dead
construction is gone and the server now logs a WARN saying reflection is unavailable.
Implementing it properly needs server streaming, which this unary-only server does not have.

Give clients the schema out of band: `grpcurl -proto service.proto ...`.

## No storage

The only server state is the `Arc<DescriptorPool>` compiled from `proto_schema` at startup. It
is immutable, never written from network bytes or handler output, and no per-client data is
retained. `GrpcProtocol` is a unit struct. This protocol has never been near the storage rule.

## Request handling

```
/package.Service/Method  ->  find descriptors  ->  decode protobuf  ->  JSON
                                                                         |
        protobuf  <-  encode  <-  grpc_unary_response.message  <-  handler
```

Event `grpc_unary_request` carries `service`, `method`, `request` (JSON), and
`expected_response_schema` — a field-name → `{type, cardinality}` map built from the response
descriptor, so the handler is told what shape to return. It declares its actions via
`.with_actions([grpc_unary_response, grpc_error])`.

### Protobuf ↔ JSON

Messages are presented to the handler as **JSON keyed by protobuf field name** — never as wire
bytes and never as hex. `{"a": 5, "b": 3}`, not a blob. Round-trip rules:

| Protobuf | JSON |
|---|---|
| numeric, bool, string, enum | native JSON; enums accept the name or the number |
| `bytes` | **base64**, in both directions |
| message | nested object |
| `repeated` | array |
| `map` | object; keys are stringified and parsed back to the declared key type |

`bytes` is the one place base64 crosses the action boundary, against the project's general
rule. It is symmetric — `Kind::Bytes` decodes what `proto_value_to_json` encoded — and the
schema hint says `"bytes (base64)"`, so there is no encode/decode asymmetry of the kind the
root CLAUDE.md warns about for `send_tcp_data`. It remains a poor fit for small models; a
schema that avoids `bytes` will work better.

Repeated and map fields **could not be produced at all** until recently: `json_to_proto_value`
switched on `field.kind()`, which for `repeated string` is `Kind::String`, so a handler
returning `{"tags": ["a","b"]}` failed with "Expected string" and the RPC came back as an
error — while `expected_response_schema` cheerfully told it the cardinality was `repeated`.
`json_to_field_value` now checks `is_map()` then `is_list()` before falling through to the
scalar path.

Numeric conversions are range-checked (`i32::try_from`) rather than truncated with `as`, and an
enum given a number is validated against the enum's declared values, matching what the
string branch already did. A field name that is not in the message is logged rather than
silently dropped.

## Error handling

`grpc_error` takes `code` and `message`, and **the code now reaches the wire.** It used to be
parsed, logged, and then folded into a `bail!` string, so every error left as
`13 INTERNAL` over HTTP 500 with the real code embedded as text inside `grpc-message`. `code`
accepts the spec spellings (`NOT_FOUND`, `INVALID_ARGUMENT`, …), lowercase, or a bare integer;
anything unrecognized becomes `2 UNKNOWN` so a typo is visible as a typo rather than
disappearing into `INTERNAL`.

**Every reply is HTTP 200.** gRPC carries application failures in `grpc-status`, not the HTTP
status line; a non-200 makes a conformant client discard `grpc-message` and synthesize
`UNAVAILABLE`. Bad path, wrong content-type, oversized body, bad frame and handler errors used
to return 404/415/400/500 respectively and now all return 200 with the right `grpc-status`:

| Condition | Status |
|---|---|
| path not `/Service/Method`, unknown service, unknown method | `12 UNIMPLEMENTED` |
| request compression flag set | `12 UNIMPLEMENTED` |
| body over 4 MiB | `8 RESOURCE_EXHAUSTED` |
| request message fails protobuf decode | `3 INVALID_ARGUMENT` |
| everything else | `13 INTERNAL` |

## Correlation

Each HTTP/2 stream is one `service_fn` future; hyper binds the returned `Response` to the
originating stream id. `handle_unary` shares no mutable state, so concurrent streams cannot
cross-talk. Nothing correlation-related needs to reach the handler.

Caveat: `connection_id` is minted per TCP connection, not per stream, and gRPC multiplexes — so
concurrent RPCs on one connection share a `connection_id` in the access log and in
`ConversationSource::Network`. Inbound gRPC metadata (`authorization`, `grpc-timeout`, trace
headers) is not parsed and does not reach the handler at all.

## Robustness

- **`grpc_error_response` no longer `unwrap()`s.** `grpc-message` is built from LLM output and
  `anyhow` chains; `HeaderValue` accepts only visible ASCII, so one non-ASCII character or
  newline made `Builder::body` return `Err` and **panicked the connection task**. Because the
  connection-cleanup code runs after `serve_connection().await` in the same task, the panic
  skipped it and left the connection permanently `Active` in `AppState`. It now falls back to
  a static message.
- **Request body capped** at 4 MiB (gRPC's own default `maxReceiveMessageLength`) via
  `http_body_util::Limited`; `req.collect()` was unbounded.
- **Frame length compared against bytes remaining**, not `5 + length`. That addition wraps on a
  32-bit target: a declared length of `0xFFFFFFFF` yields `4`, the guard passes, and
  `frame[5..4]` panics with start > end.
- **protoc output path is per-invocation.** It was the fixed
  `$TMPDIR/netget_grpc_descriptor.pb`, so two gRPC servers starting concurrently could load
  each other's schema. The file is now UUID-named and removed after reading.
- Bind uses `create_reusable_tcp_listener(...)?`; the accept-loop `JoinHandle` is registered via
  `register_server_task()`. The accept loop breaks on error rather than spinning. Per-connection
  tasks are untracked (project-wide gap).

## Known limitations

- **Unary only.** No client, server or bidirectional streaming. Extra length-prefixed frames in
  a request body are ignored without error.
- **Trailers are emitted now, and the history is worth keeping** because it is the cleanest
  example in this tree of a test suite that cannot see the bug it is sitting on. This entry
  used to say `grpc-status` is sent in the initial HEADERS alongside the DATA body and that
  grpc-go "**may** not" accept it, with "there is no Go client available here to test against".
  grpcurl 1.9.4 was installed and pointed at the server on 16 September 2026:

  ```text
  ERROR:
    Code: Internal
    Message: server closed the stream without sending trailers
  ```

  **The success and error paths differed, and the asymmetry is the whole mechanism.** A success
  has a non-empty body, so `http_body_util::Full::is_end_stream()` is false: hyper emitted
  HEADERS (no END_STREAM) then DATA (END_STREAM) and the stream ended with no trailers, which
  grpc-go rejects. An **error** has an empty body, `is_end_stream()` is true, and hyper emits a
  single HEADERS frame with END_STREAM — a valid gRPC **Trailers-Only** response, which grpc-go
  accepts and parses. So NetGet could report a *failure* to a real gRPC client and could not
  report a *success*.

  `grpc_body_with_trailers` now builds a two-frame `StreamBody` — the length-prefixed message,
  then `Frame::trailers` carrying `grpc-status` — boxed into a `BoxBody`, because `Full<Bytes>`
  cannot emit trailers at all. `grpc_error_response` is unchanged: Trailers-Only is the one
  case where the status belongs in the initial headers.

  **The caution about tonic was tested rather than assumed, and it was unfounded.** The sibling
  etcd protocol had the identical defect, is verified against the real `etcd_client` crate, and
  passes unchanged with trailers — trailers are what tonic expects too, and the header
  placement was the non-standard one. Both were fixed in the same pass.

  `tests/server/grpc/real_client_test.rs::test_grpc_unary_call_against_real_grpcurl` was
  written `#[ignore]`d against the broken server, deliberately asserting correct behaviour
  rather than the behaviour of the day. It is now un-ignored and is the regression test.
- **No reflection** (above).
- **Request compression rejected**, not decompressed.
- **No deadline enforcement** — `grpc-timeout` is ignored.
- **No auth** — no mTLS, no token checking, no metadata inspection.
- **Schema is fixed at startup.** `descriptor_pool` is an immutable `Arc` with no reload path.
- **No HTTP method check** — a `GET` with the right content-type is accepted and fails later at
  frame decode.
- The three async actions `reload_schema`, `list_services` and `describe_method` have been
  **removed**. All three built an `ActionResult::Custom` that no consumer matched, so they did
  nothing on any path, and `reload_schema` was unimplementable against an immutable pool.

## Examples

### Startup (inline proto3 text)

```
Start a gRPC server on port 50051 with this schema:

syntax = "proto3";
package calculator;
service Calculator { rpc Add(AddRequest) returns (AddResponse); }
message AddRequest { int32 a = 1; int32 b = 2; }
message AddResponse { int32 result = 1; }

Return the sum of a and b.
```

### Event

```json
{
  "event_type": "grpc_unary_request",
  "service": "calculator.Calculator",
  "method": "Add",
  "request": {"a": 5, "b": 3},
  "expected_response_schema": {"result": {"type": "int32", "cardinality": "optional"}}
}
```

### Handler response

```json
{"actions": [{"type": "grpc_unary_response", "message": {"result": 8}}]}
```

### Error

```json
{"actions": [{"type": "grpc_error", "code": "INVALID_ARGUMENT", "message": "a and b must be positive"}]}
```

## Verified

**State: Beta**, September 2026, on **grpcurl** — grpc-go, the reference implementation, and
deliberately not tonic, which this crate depends on and this server does not use.
`tests/server/grpc/real_client_test.rs` has two tests, neither `#[ignore]`d and neither
skipping when the binary is missing: one completes a unary RPC and asserts grpcurl decoded our
protobuf response field by field, the other asserts it read back our `NOT_FOUND` code and
`grpc-message`.

`tests/server/grpc/e2e_test.rs` — 5 tests covering basic unary, inline proto text, `.proto`
file loading, error responses and concurrent requests. They drive the server with hand-framed
HTTP/2 via `reqwest` (`http2_prior_knowledge`), **not** a real gRPC client, and that is why
they could not see the trailers defect: `reqwest` exposes no trailers API, so no test here can
detect it either way. `test_grpc_unary_rpc_basic` used to assert `grpc-status: 0` on the
*initial* headers, which is what kept the defect satisfied for as long as it did; it now
asserts that header is **absent** and checks the reply frame instead, leaving the status to the
client that cares where it lives.

## References

- [gRPC over HTTP/2](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md)
- [proto3 language guide](https://protobuf.dev/programming-guides/proto3/)
- [prost-reflect](https://docs.rs/prost-reflect/)
