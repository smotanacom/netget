# OpenAPI Protocol E2E Tests

## Test Overview

Tests OpenAPI 3.1 server with HTTP clients, validating spec-driven request handling, route matching, method validation,
and error responses.

## Test Strategy

**Feature-Based Tests** - Each test validates one OpenAPI capability:

1. GET endpoint (list todos)
2. POST endpoint (create todo)
3. Method validation (405 Method Not Allowed)
4. Spec compliance flag (intentional violations)
5. 404 Not Found handling

Tests use **no scripting** to ensure LLM interprets OpenAPI operations.

## LLM Call Budget

### Breakdown by Test Function

1. **`test_openapi_todo_list`** - **2 LLM calls**
    - 1 startup (spec loading)
    - 1 GET request

2. **`test_openapi_create_todo`** - **2 LLM calls**
    - 1 startup
    - 1 POST request

3. **`test_openapi_method_validation`** - **1-2 LLM calls**
    - 1 startup
    - 0-1 request (depends on llm_on_invalid flag)

4. **`test_openapi_spec_compliant_flag`** - **2 LLM calls**
    - 1 startup
    - 1 GET request (intentional violation)

5. **`test_openapi_404_not_found`** - **1-2 LLM calls**
    - 1 startup
    - 0-1 request (depends on llm_on_invalid)

**Total: 8-11 LLM calls** (within acceptable range)

## Scripting Usage

**Disabled** - Action-based mode (`ServerConfig::new_no_scripts()`):

- Each request triggers LLM call
- Validates LLM's OpenAPI interpretation
- Tests spec compliance and intentional violations

## Client Library

**reqwest** - Standard HTTP client:

- Used for: REST API calls (GET, POST, etc.)
- No specialized OpenAPI client needed

## Expected Runtime

- **Model**: qwen3-coder:30b
- **Runtime**: ~50-70 seconds for full test suite
- **Breakdown**:
    - Each test: ~10-15s (startup + 1-2 requests)
    - Spec parsing: ~500ms

## Failure Rate

**Moderate** (5-10%):

- **Stable**: Route matching, HTTP handling
- **Occasional Issues**:
    - LLM doesn't generate OpenAPI spec correctly
    - LLM returns wrong status code
    - Timeout during spec compilation

**Known Flaky Scenarios**:

- Spec parsing may fail if LLM generates invalid YAML
- 404/405 tests depend on llm_on_invalid flag behavior

## Test Cases

### 1. TODO List (`test_openapi_todo_list`)

**Validates**: GET endpoint

- Spec defines `/todos` GET operation
- Returns array of todo objects
- Status 200

### 2. Create TODO (`test_openapi_create_todo`)

**Validates**: POST endpoint

- Spec defines `/todos` POST operation
- Accepts JSON body
- Returns 200 or 201

### 3. Method Validation (`test_openapi_method_validation`)

**Validates**: 405 handling

- Spec defines POST but not GET
- GET request returns 405
- Allow header lists permitted methods

### 4. Spec Compliance Flag (`test_openapi_spec_compliant_flag`)

**Validates**: Intentional violations

- LLM returns 201 instead of 200
- Useful for testing client error handling

### 5. 404 Not Found (`test_openapi_404_not_found`)

**Validates**: Unknown path handling

- Request to undefined path returns 404
- Error response in JSON format

### 6. Body limit (`body_limit_test.rs`)

**Validates**: an oversized body is refused before the model sees it

- A 9 MiB POST is refused with 413 — the route is reachable pre-auth and `Incoming` has no
  default limit, so `req.into_body().collect()` used to buffer whatever the peer sent
- An ordinary POST on the same route still returns 201, so the guard is not simply refusing
  everything
- The `openapi_request` event is answered by a static rule, so the whole test costs one LLM
  call and an accidental extra one would show as a mock mismatch

It waits for the port to accept a connection before the large POST. `start_netget_server`
returns when startup has been *parsed*, not when the socket is bound, and a 9 MiB POST failing
that way surfaces as a transport error — which reads exactly like the cap not working. That
was observed intermittently at `--test-threads=100` before the probe was added.

## Known Issues

### Spec Generation Variability

**Issue**: LLM may generate incomplete or invalid OpenAPI specs
**Mitigation**: Tests accept placeholder responses during development
**Impact**: Tests may pass even if spec is invalid (validates server stability, not spec quality)

### Status Code Flexibility

**Issue**: LLM may return 200 instead of 201, or vice versa
**Mitigation**: Tests accept both 200 and 201 for success
**Impact**: Less strict validation than real API tests

## Test Execution

```bash
./cargo-isolated.sh build --release --all-features
./cargo-isolated.sh test --features openapi --test server::openapi::e2e_test
```

## Key Test Patterns

### Dynamic Response Handling

```rust
// Accept both structured response and placeholder
if json.get("id").is_some() {
    // Validate structured response
} else {
    println!("✓ Received placeholder response");
}
```

### Status Code Flexibility

```rust
assert!(
    status == 200 || status == 201,
    "Expected success status, got {}",
    status
);
```

### Timeout Wrapping

```rust
tokio::time::timeout(
    Duration::from_secs(20),
    client.get(url).send()
).await
```

## Why This Protocol is Complex

Compared to simpler protocols:

1. **Spec Format** - YAML/JSON OpenAPI is complex
2. **Route Matching** - Path templates with parameters
3. **Schema Validation** - Not yet implemented
4. **HTTP Semantics** - Status codes, headers, methods
5. **LLM Spec Generation** - LLM must produce valid OpenAPI

This makes tests more sensitive to LLM capabilities and increases flakiness compared to simpler RPC protocols.

## OpenAPI-Specific Features

**Route Matching Test** - Additional test file `e2e_route_matching_test.rs`:

- Tests path parameter extraction
- Validates matchit router behavior
- Ensures 404/405 logic works correctly

These tests focus on the routing layer independent of LLM responses.
