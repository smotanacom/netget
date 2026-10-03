# HTTP/HTTPS Proxy E2E Tests

## Test Overview

`certificate_cache_bound_test.rs` fills the MITM cache past its 1,024-entry bound using
CPU-only certificate generation. It asserts the bound, oldest-pair eviction, identity reuse
on cache hits, and retention of the newest certificate with its matching private key.

Comprehensive end-to-end tests for HTTP/HTTPS proxy functionality. Tests spawn real HTTP/HTTPS target servers and NetGet
proxy, then validate behavior using `reqwest` HTTP client configured to route through the proxy.

**Protocols Tested**: HTTP/1.1 proxy, HTTPS CONNECT tunneling, request/response modification, filtering

## Test Strategy

**Consolidated Test Approach**: Each test creates a target server and proxy with specific behavior, then validates
multiple operations against that instance. This minimizes server setup overhead while maintaining test isolation.

**Real Clients**: Uses `reqwest::Client` with proxy configuration, providing authentic client behavior including header
handling, chunked encoding, and connection management.

**Local Target Servers**: Tests spawn axum-based HTTP/HTTPS servers within the test process, avoiding external
dependencies and ensuring fast, reliable execution.

## LLM Call Budget

### Test Breakdown (Mock Mode - Default)

**All tests use mocks by default** - actual LLM calls: **0** in normal test runs

1. **`test_proxy_http_passthrough_with_mocks`**: Mock calls = **0 actual LLM**
2. **`test_proxy_http_block_with_mocks`**: Mock calls = **0 actual LLM**
3. **`test_proxy_https_connect_with_mocks`**: Mock calls = **0 actual LLM**
4. **`test_proxy_modify_headers_with_mocks`**: Mock calls = **0 actual LLM**
5. **`test_proxy_mitm_initialization`**: Mock calls = **0 actual LLM**
6. **`test_proxy_mitm_https_interception`**: Mock calls = **0 actual LLM**
7. **`test_proxy_mitm_request_modification`**: Mock calls = **0 actual LLM**
8. **`test_proxy_mitm_request_blocking`**: Mock calls = **0 actual LLM**
9. **`test_proxy_export_ca_certificate`**: Mock calls = **0 actual LLM**
10. **`test_proxy_mitm_response_modification_with_mocks`**: Mock calls = **0 actual LLM**
11. **`test_proxy_mitm_response_blocking_with_mocks`**: Mock calls = **0 actual LLM**

**Total: 0 LLM calls in mock mode** (default)
**Total: ~22 LLM calls with --use-ollama flag** (optional validation)

### Optimization Opportunities

**Current Issue**: Each test creates a new proxy server with specific behavior, requiring separate server startups.

**Potential Improvement**: Consolidate into 2-3 comprehensive tests:

1. **HTTP Operations Test**: Passthrough, blocking, header modification, body modification, URL rewriting in one server
2. **HTTPS Operations Test**: Passthrough, blocking by SNI in one server
3. **Path-Based Filtering Test**: Multiple paths with different actions

This could reduce to **~8 LLM calls total**, well under the 10 call target.

**Trade-off**: Less isolated tests (one failure could affect multiple assertions), but significant performance gain.

## Scripting Usage

**Scripting NOT Used**: Proxy protocol requires dynamic LLM decisions per request based on content, headers, and
context. Scripting mode doesn't provide sufficient flexibility for request inspection and modification.

Each HTTP request or HTTPS CONNECT requires LLM consultation to apply filtering rules, making per-request LLM calls
necessary.

## Client Library

**`reqwest`** v0.12 - Async HTTP client for Rust

- Built-in proxy support via `reqwest::Proxy`
- Handles CONNECT method for HTTPS tunneling automatically
- Supports custom headers, request bodies, timeouts
- Used with `danger_accept_invalid_certs(true)` for self-signed test certificates

**Target Servers**:

- **HTTP**: `axum` v0.7 - Lightweight web framework for test endpoints
- **HTTPS**: `axum-server` with `tls-rustls` feature + `rcgen` for certificate generation

## Expected Runtime

**Model**: qwen3-coder:30b (default NetGet model)

**Mock Mode (Default)**:
- **Runtime**: ~2-6 seconds for full test suite (11 tests, 0 LLM calls)
- Per-test average: ~200-500ms
- Target server startup: <100ms per test
- Proxy startup: ~500ms-1s (certificate generation in MITM tests)

**With --use-ollama Flag**:
- **Runtime**: ~110-170 seconds for full test suite (11 tests, ~22 LLM calls)
- Per-test average: ~12-15 seconds
- LLM call latency: ~2-5 seconds per call (depends on Ollama load)

**With Ollama Lock**: Tests run reliably in parallel. Total suite time with --use-ollama remains ~90-140s due to serialized LLM access.

## Failure Rate

**Historical Flakiness**: **Low** (<5%)

**Common Failure Modes**:

1. **Timeout on LLM call** (~2% of runs)
    - Symptom: Test hangs, eventually times out
    - Cause: Ollama overload or slow model inference
    - Mitigation: Ollama lock prevents this in CI

2. **Request body modification mismatch** (~1% of runs)
    - Symptom: LLM modifies request but target server doesn't receive expected changes
    - Cause: Regex replacement or header update logic inconsistency
    - Usually self-corrects on retry

3. **HTTPS certificate issues** (<1% of runs)
    - Symptom: TLS handshake failure with test HTTPS server
    - Cause: Process-specific temp file collisions (fixed with PID-based filenames)
    - Very rare now

**Most Stable Tests**:

- `test_proxy_http_passthrough`: Pass-through has no LLM decision complexity
- `test_proxy_https_passthrough`: Simple CONNECT tunnel, no inspection

**Occasionally Flaky**:

- `test_proxy_modify_request_body`: Complex body parsing and modification

## Test Cases Covered

### HTTP Functionality

1. **Basic Pass-Through** (`test_proxy_http_passthrough`)
    - Validates proxy forwards HTTP requests unchanged
    - Checks response status and body integrity
    - Verifies no header injection or modification

2. **Request Blocking** (`test_proxy_http_block`)
    - Tests LLM-controlled blocking with custom status code (403)
    - Validates custom block message in response body
    - Ensures blocked requests never reach target server

3. **Header Modification** (`test_proxy_modify_request_headers`)
    - Adds custom headers before forwarding
    - Removes specified headers (e.g., User-Agent)
    - Verifies target receives modified request

4. **Body Modification** (`test_proxy_modify_request_body`)
    - Tests POST request body pass-through
    - Validates body integrity (no corruption)
    - Foundation for future body transformation tests

5. **Path-Based Filtering** (`test_proxy_filter_by_path`)
    - Blocks specific paths (/json) while allowing others (/)
    - Tests selective filtering configuration
    - Validates pattern matching correctness

6. **URL Rewriting** (`test_proxy_url_rewrite`)
    - Rewrites request paths before forwarding
    - Tests /api/* → / transformation
    - Validates target receives rewritten path

### HTTPS Functionality

7. **HTTPS Pass-Through (CONNECT)** (`test_proxy_https_passthrough`)
    - Validates HTTPS CONNECT tunneling without decryption
    - Tests against local self-signed HTTPS server
    - Verifies end-to-end encrypted connection

8. **HTTPS Blocking by SNI** (`test_proxy_https_block_by_sni`)
    - Blocks HTTPS connections based on destination host
    - Tests LLM-controlled allow/block decisions
    - Validates 403 response for blocked destinations

### MITM Mode Tests

9. **`test_proxy_mitm_initialization`**: Verifies proxy starts in MITM mode with certificate generation
10. **`test_proxy_mitm_https_interception`**: Tests HTTPS request decryption and inspection
11. **`test_proxy_mitm_request_modification`**: Validates header injection in decrypted HTTPS
12. **`test_proxy_mitm_request_blocking`**: Tests blocking based on decrypted HTTPS content
13. **`test_proxy_export_ca_certificate`**: Validates CA certificate export for user installation
14. **`test_proxy_mitm_response_modification_with_mocks`**: Tests response modification in MITM mode (status, headers)
15. **`test_proxy_mitm_response_blocking_with_mocks`**: Tests response blocking in MITM mode

### Coverage Gaps

**Not Yet Tested**:

- Certificate caching behavior (cache hits, expiration)
- WebSocket upgrade handling
- Chunked transfer encoding edge cases
- Multiple concurrent HTTPS connections to same proxy (stress testing)
- Proxy authentication (not implemented)
- IPv6 target addresses
- MITM with loaded CA certificate (from file)

## Test Infrastructure

### Helper Functions

**`start_test_http_server()`**:

- Spawns axum HTTP server on random port
- Routes: `/` (root), `/echo` (header inspection), `/json` (JSON response), `/post` (POST endpoint)
- Returns `(port, join_handle)` for cleanup

**`start_test_https_server()`**:

- Generates self-signed certificate with rcgen
- Spawns axum-server with rustls TLS
- Uses PID-based temp files to avoid concurrent test conflicts
- Returns `(port, join_handle)`

**`helpers::start_netget_server()`**:

- Spawns NetGet proxy with prompt
- Waits for server startup
- Returns `ServerState` with port and stop handle

### Test Execution Pattern

```rust
// 1. Start target server
let (target_port, _handle) = start_test_http_server().await?;

// 2. Start NetGet proxy with behavior prompt
let proxy_port = 0; // discover the bound port from server.port below
let prompt = format!("listen on port {} using proxy stack. <behavior>", proxy_port);
let server = helpers::start_netget_server(ServerConfig::new(prompt)).await?;

// 3. Configure HTTP client to use proxy
let proxy_url = format!("http://127.0.0.1:{}", server.port);
let proxy = reqwest::Proxy::all(&proxy_url)?;
let client = reqwest::Client::builder().proxy(proxy).build()?;

// 4. Make request through proxy to target
let target_url = format!("http://127.0.0.1:{}/", target_port);
let response = client.get(&target_url).send().await?;

// 5. Validate response
assert_eq!(response.status(), 200);

// 6. Cleanup
server.stop().await?;
```

## Known Issues

### Timing-Sensitive Tests

**Issue**: Some tests have tight timing windows (e.g., 500ms startup delay)
**Impact**: Occasional flakiness on slow CI runners
**Mitigation**: Use `helpers::wait_for_server_startup()` with dynamic port checking

### HTTPS Certificate Trust

**Issue**: reqwest requires `danger_accept_invalid_certs(true)` for self-signed test certificates
**Impact**: Test behavior differs from production (where certs should be validated)
**Mitigation**: Acceptable for E2E tests; production validation tested separately

### LLM Response Variability

**Issue**: LLM may phrase error messages differently across runs
**Impact**: Tests check for substrings (e.g., "Access Denied") rather than exact matches
**Mitigation**: Use flexible assertion patterns: `assert!(body.contains("Denied"))`

## Running Tests

```bash
# Run all proxy tests in mock mode (no Ollama required, fast)
./test-e2e.sh proxy
# OR
./cargo-isolated.sh test --features proxy --test server::proxy::e2e_test

# Run with real Ollama (for validation)
./test-e2e.sh --use-ollama proxy
# OR
./cargo-isolated.sh test --features proxy --test server::proxy::e2e_test -- --use-ollama

# Run specific MITM test
./cargo-isolated.sh test --features proxy --test server::proxy::e2e_test test_proxy_mitm_initialization

# Run with output
./cargo-isolated.sh test --features proxy --test server::proxy::e2e_test -- --nocapture

# Run with concurrency (mocks are thread-safe)
./cargo-isolated.sh test --features proxy --test server::proxy::e2e_test -- --test-threads=100
```

## Future Test Additions

1. **Certificate Cache Validation**: Test cache hits, expiration, domain normalization
2. **MITM with Loaded CA**: Test loading CA certificate from file instead of generating
3. **Concurrent HTTPS Requests**: Spawn multiple HTTPS clients through MITM simultaneously
4. **Large Request Bodies in MITM**: Test streaming and chunked encoding through TLS
5. **Proxy Chaining**: NetGet MITM proxy → another proxy → target
6. **Error Handling**: TLS handshake failures, invalid certificates, upstream TLS errors
7. **Performance Benchmarking**: Measure MITM overhead vs pass-through mode
8. **Certificate SAN Validation**: Verify wildcard and www variants in generated certs
9. **Response Body Modification in MITM**: Test detailed body content replacement and regex patterns
