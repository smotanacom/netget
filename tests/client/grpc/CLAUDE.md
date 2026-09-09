# gRPC Client E2E Testing

## Test Strategy

Black-box E2E testing that spawns NetGet gRPC server and client instances, verifies RPC calls work correctly with
dynamic schema support.

## Test Approach

### Server Setup

- Spawn NetGet gRPC server with base64-encoded FileDescriptorSet
- Server implements calculator.Calculator service with Add RPC
- LLM handles server-side logic (returns sum of a and b)

### Client Testing

- Spawn NetGet gRPC client with same schema
- Client makes RPC calls based on LLM instructions
- Verify client can connect, make calls, and handle responses

### Schema Used

Calculator service with simple Add operation:

- Service: `calculator.Calculator`
- Method: `Add(AddRequest) returns (AddResponse)`
- Request: `{a: int32, b: int32}`
- Response: `{result: int32}`

Schema provided as base64 FileDescriptorSet (no protoc dependency).

## LLM Call Budget

**Target: < 10 LLM calls per test suite**

### Current Budget

- `test_grpc_client_add_request`: 2 LLM calls
    1. Server startup (parse instruction, generate schema handler)
    2. Client connection and RPC call
- `test_grpc_client_connection_error`: 1 LLM call
    1. Client connection attempt

**Total: 3 LLM calls**

### Budget Rationale

- **Minimal**: 2 tests cover core functionality (success + error)
- **Efficient**: Reuse same schema across tests
- **Simple service**: Calculator is trivial, LLM handles easily
- **Fast**: Tests complete in < 3 seconds

## Expected Runtime

- `test_grpc_client_add_request`: ~3 seconds
    - 1s server startup
    - 2s client connection + RPC call
- `test_grpc_client_connection_error`: ~1 second
    - 1s connection attempt timeout

**Total suite runtime: ~4 seconds**

## Test Coverage

### Covered

✅ gRPC client initialization with dynamic schema
✅ RPC method call (unary)
✅ Request/response JSON ↔ protobuf conversion
✅ Connection error handling
✅ End-to-end server ↔ client communication

✅ Repeated and map request fields, and a wrong-typed scalar being refused
  (`command_channel_test.rs`)

### Not Covered (Future Tests)

❌ Streaming RPCs (not implemented)
❌ gRPC metadata (headers)
❌ TLS connections
❌ Multiple services in one schema
❌ Nested messages and enums in requests
❌ Error status codes from server — and in particular a server that puts `grpc-status` in
  HTTP/2 **trailers**, which every real gRPC server does and NetGet's own gRPC server does
  not. The trailer path in `call_grpc_unary` is therefore unexercised by any test here; it
  needs a non-NetGet peer

## Known Issues

### Schema Encoding

- Base64 FileDescriptorSet must be correct
- If schema is malformed, client fails to initialize
- Error messages may be cryptic ("Failed to create descriptor pool")

### Timing

- Tests use sleep() for synchronization (brittle)
- If LLM is slow, tests may timeout
- Server needs ~1s to fully initialize

### LLM Variability

- LLM may not always call the RPC method immediately
- LLM may log extra messages (affects output verification)
- Tests check for multiple possible output patterns

## Running Tests

```bash
# Run gRPC client E2E tests only
./cargo-isolated.sh test --no-default-features --features grpc --test client::grpc::e2e_test

# Run with output
./cargo-isolated.sh test --no-default-features --features grpc --test client::grpc::e2e_test -- --nocapture
```

## Dependencies

- **tonic**: gRPC client library
- **prost-reflect**: Dynamic protobuf schema
- **NetGet gRPC server**: For E2E testing (same codebase)

## Future Improvements

1. **Real gRPC server**: Use external gRPC service (e.g., grpc.io examples)
2. **More complex schemas**: Test nested messages, repeated fields, enums
3. **Streaming**: Once implemented, test all 4 RPC types
4. **Metadata testing**: Verify custom headers work
5. **Error codes**: Test all gRPC status codes (INVALID_ARGUMENT, NOT_FOUND, etc.)

## `command_channel_test.rs` — injected actions

In-process, no NetGet subprocess and **zero LLM calls**: a NetGet gRPC *server* with the
calculator schema answers through a `*` static handler, and the client's LLM points at
`http://127.0.0.1:1`, so its `grpc_connected` call fails and the connect path has to tolerate
that.

Needs `protoc` on PATH — the gRPC server compiles its `proto_schema` by shelling out to it on
every code path. The test skips itself with a message when `protoc` is absent rather than
failing, because CI does not install it and does not build the `grpc` feature either.

What it pins:

- `has_client_handle` is true **before** anything answers the connected event.
- `call_grpc_method calculator.Calculator/Add {a:5,b:3}` → `Sent { bytes_sent: 9 }`. That is
  the one honest `Sent` in this family of clients: NetGet builds the gRPC frame itself, so
  9 = 5-byte header + `0x08 0x05 0x10 0x03`. The server's access log shows `Add`.
- an unknown action → `Rejected`; `disconnect` → `Disconnected`, handle gone.

**LLM call budget: 0.**
