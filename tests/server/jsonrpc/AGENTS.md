# JSON-RPC Protocol E2E Tests

## Test Overview

Tests JSON-RPC 2.0 server with HTTP clients, validating single requests, notifications, batch requests, and error
handling per the JSON-RPC 2.0 specification.

## Test Strategy

**Consolidated Tests per Feature** - Each test validates one JSON-RPC 2.0 feature:

1. Basic method call (single request/response)
2. Notification (no response expected)
3. Batch request (array of requests)
4. Method not found error

Tests use **action-based mode** (no scripting) to ensure LLM interprets each request individually.

## LLM Call Budget

### Breakdown by Test Function

1. **`test_jsonrpc_basic_method_call`** - **2 LLM calls**
    - 1 startup call to understand server behavior
    - 1 method call (add)
    - Total: 2 LLM calls

2. **`test_jsonrpc_notification`** - **2 LLM calls**
    - 1 startup call
    - 1 notification (log_event) - still calls LLM but no response
    - Total: 2 LLM calls

3. **`test_jsonrpc_batch_request`** - **5 LLM calls**
    - 1 startup call
    - 3 batch items (echo method called 3 times)
    - Each batch item processed separately by LLM
    - Total: 4 LLM calls (1 startup + 3 requests)

4. **`test_jsonrpc_method_not_found`** - **2 LLM calls**
    - 1 startup call
    - 1 method call (unknown method)
    - Total: 2 LLM calls

5. **`llm_failure_test::test_jsonrpc_refuses_oversized_input_and_fails_closed_with_a_category`**
   — **1 LLM call**
    - 1 startup call. The two refusals (oversized body, oversized batch) are answered before
      any model is consulted, and the third request deliberately matches no mock rule so the
      mock backend answers HTTP 500 and `call_llm` returns `Err` — the branch under test.
    - Total: 1 LLM call

**Total: 11 LLM calls** across both files.

## Scripting Usage

**Disabled** - Tests use action-based mode:

- Each JSON-RPC request triggers LLM call
- Validates LLM's ability to parse JSON-RPC format
- Tests error handling (LLM must return correct error codes)

**Why No Scripting?**

- JSON-RPC is dynamic (method names determined at runtime)
- Scripting would bypass testing LLM's JSON-RPC interpretation
- Error cases require LLM decision-making

## Client Library

**reqwest** - Standard HTTP client:

- Used for: HTTP POST requests with JSON body
- No specialized JSON-RPC client needed (simple HTTP+JSON)
- Manual JSON-RPC request construction validates protocol understanding

## Expected Runtime

- **Model**: qwen3-coder:30b (or any model)
- **Runtime**: ~40-60 seconds for full test suite
- **Breakdown**:
    - Basic method call: ~15s (startup + 1 request)
    - Notification: ~12s (startup + notification)
    - Batch request: ~25s (startup + 3 requests processed sequentially)
    - Method not found: ~10s (startup + error case)

## Failure Rate

**Low** (2-5%):

- **Stable**: JSON parsing, HTTP handling
- **Occasional Issues**:
    - LLM returns wrong error code (e.g., -32603 instead of -32601)
    - LLM doesn't include `id` in response
    - Timeout during batch processing (3 sequential LLM calls)

**Known Flaky Scenarios**:

- Batch test may timeout if Ollama is overloaded (3 requests back-to-back)
- LLM may misunderstand notification semantics (tries to return result)

## Test Cases

### 1. Basic Method Call (`test_jsonrpc_basic_method_call`)

**Validates**: Single request/response

- POST with JSON-RPC 2.0 request (`method: "add"`)
- Response has `jsonrpc: "2.0"`, matching `id`
- Response has either `result` or `error` (not both)
- HTTP 200 status

### 2. Notification (`test_jsonrpc_notification`)

**Validates**: No-response notification

- JSON-RPC request without `id` field
- HTTP 200 or 204 response
- No response body (or empty)

### 3. Batch Request (`test_jsonrpc_batch_request`)

**Validates**: Array of requests

- POST with JSON array of 3 requests
- Response is JSON array
- Each response has `jsonrpc: "2.0"` and `id`
- Array length matches request count (excluding notifications)

### 4. Method Not Found (`test_jsonrpc_method_not_found`)

**Validates**: Error handling

- POST with unknown method
- HTTP 200 (JSON-RPC always returns 200)
- Response has `error` object
- Error has `code` (should be -32601) and `message`
- Error code is negative

### 5. Refusals and fail-closed (`llm_failure_test.rs`)

**Validates**: the two input caps and what a caller gets when netget itself cannot answer.

- A body over `MAX_REQUEST_BODY_BYTES` (4 MiB) → `-32600` naming the limit. `req.collect()`
  was unbounded, and the body is buffered whole, parsed and pretty-printed into the trace log.
- A batch over `MAX_BATCH_LEN` (128) → `-32600` naming the limit. Every member is a separate
  sequential model call, so batch length is an amplification factor the body cap does **not**
  bound — at forty bytes a member, 4 MiB is a hundred thousand model calls from one POST.
- A failed LLM call → `-32603` (or `-32000` with `data.retryable = true` when the backend is
  merely saturated), with the message from `WireFailure::text()` and the request `id` echoed
  preserving its JSON type. The test asserts the two codes agree with `data.retryable`, and
  greps the raw body for `http://`, `ollama`, `11434`, `retries`, `.rs:`, `anyhow` and
  `/Users/` — anything derived from the error reaching the wire is the defect it exists for.

The two caps are duplicated as constants at the top of the test file; they must be kept in
step with `src/server/jsonrpc/mod.rs`.

## Known Issues

### Error Code Variance

**Issue**: LLM may return different error codes than expected

- Expected: -32601 (Method not found)
- Actual: May return -32603 (Internal error) or other codes

**Mitigation**: Test accepts any negative error code
**Impact**: Not a test failure, indicates LLM interpretation variance

### Batch Processing Timeout

**Issue**: 3 sequential LLM calls in batch test can timeout
**Mitigation**: 30-second timeout for batch requests
**Impact**: Occasional failures if Ollama is slow

## Test Execution

```bash
# Build release binary with all features
./cargo-isolated.sh build --release --all-features

# Run JSON-RPC tests
./cargo-isolated.sh test --features jsonrpc --test server::jsonrpc::e2e_test

# Run specific test
./cargo-isolated.sh test --features jsonrpc --test server::jsonrpc::e2e_test test_jsonrpc_basic_method_call
```

## Key Test Patterns

### JSON-RPC Request Construction

```rust
let request_body = json!({
    "jsonrpc": "2.0",
    "method": "add",
    "params": [5, 3],
    "id": 1
});
```

### Response Validation

```rust
// Must have jsonrpc field
assert_eq!(json.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));

// Must have matching id
assert_eq!(json.get("id"), Some(&json!(1)));

// Must have result XOR error
let has_result = json.get("result").is_some();
let has_error = json.get("error").is_some();
assert!(has_result || has_error);
assert!(!(has_result && has_error));
```

### Batch Request Validation

```rust
let responses = json.as_array().expect("Batch response should be an array");
for (i, resp) in responses.iter().enumerate() {
    assert_eq!(resp.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));
    assert!(resp.get("id").is_some());
}
```

## Why This Protocol is Simple

Compared to other RPC protocols:

1. **Text-based** - JSON is human-readable and easy for LLM
2. **Simple spec** - Only 3 message types (request, batch, notification)
3. **Standard errors** - Well-defined error codes
4. **No schema** - Methods and params are freeform
5. **HTTP transport** - No complex framing like gRPC

This makes tests **highly reliable** and LLM interpretation straightforward.

## JSON-RPC 2.0 Compliance Notes

**Strict Compliance**:

- ✅ Version field (`jsonrpc: "2.0"`) required and validated
- ✅ `id` matching between request and response
- ✅ Error codes follow specification
- ✅ Batch request handling

**Minor Deviations**:

- Response order in batch may not match request order (spec allows this)
- Notification returns 204 instead of 200 (both acceptable)

Tests validate compliance where specification is strict, and accept variations where spec is flexible.
