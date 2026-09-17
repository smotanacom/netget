# gRPC Protocol E2E Tests

## Test Overview

Tests gRPC server with dynamic protobuf schemas using real HTTP/2 clients and protobuf encoding. Validates schema
loading, unary RPC handling, error responses, and concurrent requests.

## The maturity evidence is `real_client_test.rs`, not this suite

`DevelopmentState::Beta` rests on **grpcurl** — grpc-go, the reference implementation, and
deliberately not tonic, which this crate depends on and this server does not use, so the
evidence is not circular. Two tests, neither `#[ignore]`d and neither skipping when the binary
is absent (they fail, naming `brew install grpcurl`): one completes a unary RPC and asserts
grpcurl decoded our protobuf response field by field, the other asserts it read back our
`NOT_FOUND` code and `grpc-message`.

**Everything else here is `reqwest` with `http2_prior_knowledge`, which is not a gRPC client.**
It hand-frames the 5-byte prefix and posts the bytes. That is fine for schema loading and
routing, and it is structurally unable to check where the status lives, because reqwest exposes
no trailers API.

That gap hid a defect that broke **every successful RPC** against a real client. Until
September 2026 the server wrote `grpc-status` into the initial HEADERS and emitted no trailers;
a success has a non-empty body, so the stream ended after DATA with nothing, and grpcurl
answered `Internal: server closed the stream without sending trailers` to every call. Errors
were unaffected — an empty body makes them Trailers-Only by accident — so **only the success
path was broken and the failure paths were the ones being asserted on**. `test_grpc_unary_rpc_basic`
asserted `grpc-status: 0` *on the initial headers*, which is precisely the assertion that kept
the bug satisfied; it now asserts that header is absent and checks the reply frame instead.

The success test in `real_client_test.rs` was written `#[ignore]`d against the broken server,
describing correct behaviour rather than the behaviour of the day. It is now un-ignored and is
the regression test.

```bash
./cargo-isolated.sh test --no-default-features --features grpc \
    --test server -- server::grpc --test-threads=8
```

`grpc` needs `protoc` on PATH to build and to compile a schema at startup.

## Test Strategy

**Consolidated Tests with Schema Reuse** - Each test validates one aspect of gRPC:

1. Basic unary RPC with inline proto text
2. Proto file loading from disk
3. Inline proto text compilation
4. Error response handling
5. Concurrent request processing

Tests use **no scripting** (action-based mode) to ensure LLM interprets each request.

### Important Limitations

**Base64-encoded FileDescriptorSet is NOT supported** - LLMs truncate long strings (>1500 chars) in JSON responses,
making base64 FileDescriptorSet unreliable. Use one of these approaches instead:

- ✅ **File path** - Provide path to .proto file
- ✅ **Inline proto text** - Include proto3 definition in prompt
- ❌ **Base64 FileDescriptorSet** - Will be truncated by LLM

## LLM Call Budget

### Breakdown by Test Function

1. **`test_grpc_unary_rpc_basic`** - **1 LLM call**
    - 1 startup call to understand schema and behavior
    - 1 unary RPC call (GetUser)
    - Total: 2 LLM calls (counting startup)

2. **`test_grpc_proto_file_loading`** - **1 LLM call**
    - 1 startup call to load schema from file
    - No actual RPC made (validates schema loading only)
    - Total: 1 LLM call

3. **`test_grpc_proto_text_inline`** - **1 LLM call**
    - 1 startup call to compile inline proto text
    - No actual RPC made (validates compilation)
    - Total: 1 LLM call

4. **`test_grpc_error_response`** - **2 LLM calls**
    - 1 startup call
    - 1 unary RPC call (error case)
    - Total: 2 LLM calls

5. **`test_grpc_concurrent_requests`** - **4 LLM calls**
    - 1 startup call
    - 3 concurrent unary RPC calls
    - Total: 4 LLM calls

**Total: 10 LLM calls** (exactly at limit)

**Note**: Ollama lock ensures concurrent requests are serialized at API level, preventing overload.

## Scripting Usage

**Disabled** - Tests use `ServerConfig::new_no_scripts()`:

- Ensures LLM interprets every request
- Validates action-based response handling
- Prevents script/protocol event_type_id mismatches

## Client Library

**Manual HTTP/2 + prost-reflect** - No high-level gRPC client:

- `reqwest` with `http2_prior_knowledge()` for HTTP/2 transport
- `prost-reflect` for dynamic protobuf encoding/decoding
- Manual gRPC framing (5-byte header + payload)

**Why Not tonic Client?**

- tonic requires compile-time code generation
- Dynamic schema approach requires runtime message construction
- prost-reflect provides necessary flexibility

## Expected Runtime

- **Model**: qwen3-coder:30b (or any model that can interpret protobuf schemas)
- **Runtime**: ~60-90 seconds for full test suite
- **Breakdown**:
    - Basic unary RPC: ~20s (startup + 1 RPC)
    - Proto file loading: ~15s (startup only)
    - Proto text inline: ~20s (startup with compilation)
    - Error response: ~20s (startup + error RPC)
    - Concurrent requests: ~30s (startup + 3 concurrent RPCs with serialization)

**Slowdown Factors**:

- protoc compilation (if not cached)
- Ollama lock serialization for concurrent test
- LLM schema interpretation

## Failure Rate

**Low to Moderate** (5-10%):

- **Stable**: Schema loading, compilation
- **Occasional Issues**:
    - protoc not found in PATH (skips test gracefully)
    - LLM schema interpretation errors (complex proto types)
    - Timeout during concurrent requests (if Ollama is slow)

**Known Flaky Scenarios**:

- Empty response from LLM (returns default empty message)
- Concurrent test may have 1-2 failures out of 3 requests (acceptable per test logic)

## Test Cases

### 1. Basic Unary RPC (`test_grpc_unary_rpc_basic`)

**Validates**: Core gRPC functionality

- Inline proto text schema loading
- GetUser unary RPC call
- Protobuf request encoding (UserId with id=123)
- HTTP/2 transport with gRPC framing
- `grpc-status: 0` header for success

### 2. Proto File Loading (`test_grpc_proto_file_loading`)

**Validates**: File-based schema loading

- Reads .proto file from temp directory
- protoc compilation triggered by server
- Server starts successfully
- Schema loaded message in output

### 3. Proto Text Inline (`test_grpc_proto_text_inline`)

**Validates**: Inline schema compilation

- Provides raw proto3 text in prompt
- Server compiles with protoc
- Server remains running after compilation
- No startup errors

### 4. Error Response (`test_grpc_error_response`)

**Validates**: gRPC error handling

- GetUser with id=0 triggers NOT_FOUND error
- LLM returns `grpc_error` action
- HTTP 200 with `grpc-status` header indicating error
- Server remains stable after error

### 5. Concurrent Requests (`test_grpc_concurrent_requests`)

**Validates**: Concurrent connection handling

- 3 simultaneous GetUser calls with different IDs
- Ollama lock ensures serialized LLM calls
- At least 2/3 requests succeed (graceful degradation)
- No connection interference

## Known Issues

### protoc Dependency

**Issue**: Tests fail if protoc not found
**Mitigation**: Test checks for protoc availability and panics with installation instructions
**Impact**: CI/CD environments MUST have protoc installed (brew install protobuf or apt-get install protobuf-compiler)

### Concurrent Test Flakiness

**Issue**: Concurrent requests may timeout if Ollama is overloaded
**Mitigation**: Test accepts 2/3 success rate
**Impact**: Occasional failures are expected and acceptable

### Empty LLM Responses

**Issue**: If LLM doesn't return action, server returns empty message
**Mitigation**: Test verifies server stability, not response content
**Impact**: Doesn't fail test, but indicates LLM interpretation issue

## Test Execution

```bash
# Install protoc (required for tests)
# macOS: brew install protobuf
# Ubuntu: apt-get install protobuf-compiler

# Build release binary with all features
./cargo-isolated.sh build --release --all-features

# Run gRPC tests
./cargo-isolated.sh test --features grpc --test server::grpc::e2e_test

# Run specific test
./cargo-isolated.sh test --features grpc --test server::grpc::e2e_test test_grpc_unary_rpc_basic

# Skip tests if protoc not available (tests auto-skip)
./cargo-isolated.sh test --features grpc --test server::grpc::e2e_test
```

## Key Test Patterns

### protoc Availability Check

```rust
if std::process::Command::new("protoc").arg("--version").output().is_err() {
    panic!("protoc not found in PATH. Please install protobuf compiler: brew install protobuf (macOS) or apt-get install protobuf-compiler (Linux)");
}
```

Tests will FAIL (not skip) if protoc is missing.

### Manual gRPC Framing

```rust
// Encode gRPC frame: 1 byte compression + 4 bytes length + payload
let mut grpc_frame = vec![0u8];  // No compression
grpc_frame.extend_from_slice(&(request_body.len() as u32).to_be_bytes());
grpc_frame.extend_from_slice(&request_body);
```

### Process-Specific Temp Files

```rust
let pid = std::process::id();
let proto_file = temp_dir.join(format!("test_grpc_{}.proto", pid));
```

Prevents conflicts when running tests concurrently.

## Why This Protocol is Challenging

Unlike simpler protocols:

1. **Complex schema format** - Protobuf is harder for LLM to generate
2. **Binary encoding** - Requires JSON ↔ protobuf translation
3. **HTTP/2 requirement** - More complex transport than HTTP/1.1
4. **Type system** - LLM must understand protobuf types
5. **protoc dependency** - External tool requirement

This makes tests more sensitive to LLM interpretation accuracy.
