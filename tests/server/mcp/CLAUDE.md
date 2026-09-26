# MCP (Model Context Protocol) E2E Tests

## Test Overview

Tests MCP server with JSON-RPC 2.0 clients, validating initialize flow, resources, tools, prompts, ping, and error
handling per the MCP specification.

## Test Strategy

**Feature-Based Tests** - Each test validates one MCP capability:

1. Initialize handshake
2. Ping (health check)
3. Resources list
4. Resources read
5. Tools list
6. Tools call
7. Prompts list
8. Prompts get
9. Error handling (unknown method)

Tests use **action-based mode** to ensure LLM interprets MCP semantics.

## LLM Call Budget

### Breakdown by Test Function

1. **`test_mcp_initialize`** - **1 LLM call**
    - 1 initialize request

2. **`test_mcp_ping`** - **0 LLM calls**
    - Ping is hardcoded (no LLM call)

3. **`test_mcp_resources_list`** - **2 LLM calls**
    - 1 startup + 1 resources/list

4. **`test_mcp_resources_read`** - **2 LLM calls**
    - 1 startup + 1 resources/read

5. **`test_mcp_tools_list`** - **2 LLM calls**
    - 1 startup + 1 tools/list

6. **`test_mcp_tools_call`** - **2 LLM calls**
    - 1 startup + 1 tools/call

7. **`test_mcp_prompts_list`** - **2 LLM calls**
    - 1 startup + 1 prompts/list

8. **`test_mcp_prompts_get`** - **2 LLM calls**
    - 1 startup + 1 prompts/get

9. **`test_mcp_error_handling`** - **1 LLM call**
    - 1 startup (invalid method returns error without LLM call)

**Total: 14 LLM calls** (slightly over limit but tests are independent)

**Note**: Tests can be consolidated to reuse server instances if budget needs to be reduced.

## Scripting Usage

**Disabled** - Action-based mode:

- Each MCP request triggers LLM call
- Validates LLM's MCP interpretation
- Tests capability declaration and implementation

## Client Library

**reqwest + Manual JSON-RPC** - No specialized MCP client:

- `reqwest` for HTTP POST
- Manual JSON-RPC 2.0 message construction
- Helper function `send_mcp_request()` for request/response handling

**Why No MCP Client Library?**

- No mature Rust MCP client exists
- Manual construction validates protocol understanding
- Simple HTTP + JSON-RPC doesn't require library

## Expected Runtime

- **Model**: qwen3-coder:30b
- **Runtime**: ~70-100 seconds for full test suite
- **Breakdown**:
    - Each test: ~8-12s (startup + 1-2 requests)
    - Initialize: ~10s (LLM interprets capability system)

## Failure Rate

**Moderate** (5-10%):

- **Stable**: JSON-RPC handling, HTTP transport
- **Occasional Issues**:
    - LLM doesn't return proper capability structure in initialize
    - LLM returns empty resources/tools/prompts lists
    - Timeout on slower models or complex capability generation

**Known Flaky Scenarios**:

- Initialize may fail if LLM doesn't understand MCP capability structure
- Resource/tool/prompt requests may return "not found" errors (acceptable per tests)

## Test Cases

### 1. Initialize (`test_mcp_initialize`)

**Validates**: MCP handshake

- Send initialize request with clientInfo
- Receive response with protocolVersion, capabilities, serverInfo
- Protocol version is "2024-11-05"

### 2. Ping (`test_mcp_ping`)

**Validates**: Health check

- Send ping request
- Receive empty success response
- No errors

### 3. Resources List (`test_mcp_resources_list`)

**Validates**: Resource discovery

- Send resources/list request
- Receive array of resources with uri, name, description

### 4. Resources Read (`test_mcp_resources_read`)

**Validates**: Resource content retrieval

- Send resources/read with uri
- Receive contents array with resource data
- Accept "not found" error (indicates proper error handling)

### 5. Tools List (`test_mcp_tools_list`)

**Validates**: Tool discovery

- Send tools/list request
- Receive array of tools with name, description, inputSchema

### 6. Tools Call (`test_mcp_tools_call`)

**Validates**: Tool execution

- Send tools/call with name and arguments
- Receive content array with result
- Accept "execution error" (indicates proper error handling)

### 7. Prompts List (`test_mcp_prompts_list`)

**Validates**: Prompt template discovery

- Send prompts/list request
- Receive array of prompts with name, description

### 8. Prompts Get (`test_mcp_prompts_get`)

**Validates**: Prompt template retrieval

- Send prompts/get with name
- Receive messages array with prompt template
- Accept "not found" error

### 9. Error Handling (`test_mcp_error_handling`)

**Validates**: Unknown method handling

- Send invalid/method request
- Receive error response with code and message
- Error structure matches JSON-RPC 2.0

### 10. Inbound bound (`inbound_limit_test.rs`)

In-process on `tests/helpers/inbound_limit.rs` (a mock model that answers with no actions and
counts calls), raw HTTP/1.1: a `tools/list` body of exactly `MAX_REQUEST_BODY_BYTES` reaches
the model; the same one byte longer is answered 413 with the fixed JSON-RPC error and zero
model calls, both with a `Content-Length` and chunked; a fresh request is still served.
Verified by removal: with `DefaultBodyLimit::disable()` in place of the explicit limit the
over-limit body was answered 200 by the model.

## Known Issues

### Capability Structure Complexity

**Issue**: LLM may not generate proper capability structure
**Mitigation**: Tests validate structure but accept variations
**Impact**: Tests pass even if capabilities are incomplete

### Empty Lists Acceptable

**Issue**: LLM may return empty resources/tools/prompts
**Mitigation**: Tests accept empty lists as valid
**Impact**: Doesn't validate actual capability implementation

### "Not Found" Errors Expected

**Issue**: Resource read and prompt get may return errors
**Mitigation**: Tests explicitly accept error responses
**Impact**: Tests validate error handling, not successful retrieval

## Test Execution

```bash
./cargo-isolated.sh build --release --all-features
./cargo-isolated.sh test --features mcp --test server::mcp::e2e_test

# Run specific capability test
./cargo-isolated.sh test --features mcp --test server::mcp::e2e_test test_mcp_tools_list
```

## Key Test Patterns

### Helper Function for Requests

```rust
async fn send_mcp_request(
    port: u16,
    method: &str,
    params: Option<Value>,
    id: Option<i64>,
) -> E2EResult<Value>
```

### Response Validation

```rust
assert_eq!(response.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));
assert_eq!(response.get("id"), Some(&json!(1)));

if let Some(result) = response.get("result") {
    // Success case
} else {
    // Error case - may be acceptable
}
```

### Timeout Wrapping

```rust
tokio::time::timeout(
    Duration::from_secs(30),
    client.post(url).json(&request_body).send()
).await
```

## Why This Protocol is Advanced

Compared to simpler protocols:

1. **Capability System** - Complex initialization with capability negotiation
2. **Three-Phase Handshake** - Initialize → response → initialized notification
3. **Multiple Concepts** - Resources, tools, prompts all distinct
4. **Schema Definitions** - Tools require JSON Schema for inputs
5. **Session Management** - State persists across requests

This makes tests more sensitive to LLM understanding of MCP concepts.

## MCP-Specific Features

**Session Tracking**:

- Tests don't explicitly track sessions (handled server-side)
- Each test creates new session via initialize
- No session cleanup tested (future enhancement)

**Notification Handling**:

- initialized notification not tested (server accepts but doesn't require)
- Progress/cancelled notifications not tested

**Subscription System**:

- Resource subscriptions declared but not tested
- No change notifications validated
