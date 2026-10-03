# Elasticsearch Protocol E2E Tests

## Test Overview

Tests Elasticsearch-compatible server implementation using HTTP client (`reqwest`). Validates search, document
operations (index, get, delete), bulk operations, cluster health, and root endpoint. Does NOT use official Elasticsearch
client to keep tests simple.

## Test Strategy

**HTTP-Based Testing**: Uses raw HTTP client to test REST API directly. Sends requests with appropriate HTTP methods (
GET, POST, PUT, DELETE) and paths. This approach:

- Tests HTTP protocol directly
- Validates JSON request/response format
- Simpler than using official Elasticsearch client
- More maintainable and debuggable

**Comprehensive Coverage**: Multiple small tests, each testing one operation type. Covers full REST API surface.

## LLM Call Budget

### Test: `test_elasticsearch_search`

- **1 server startup**
- **1 search request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_elasticsearch_index_document`

- **1 server startup**
- **1 index document request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_elasticsearch_get_document`

- **1 server startup**
- **1 get document request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_elasticsearch_bulk_operations`

- **1 server startup**
- **1 bulk request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_elasticsearch_cluster_health`

- **1 server startup**
- **1 cluster health request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_elasticsearch_root_endpoint`

- **1 server startup**
- **1 root endpoint request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_elasticsearch_delete_document`

- **1 server startup**
- **1 delete document request** → LLM call
- **Total: 2 LLM calls**

**Total for Elasticsearch test suite: 14 LLM calls** (exceeds 10 limit)

## Scripting Usage

**Scripting Disabled**: All tests use `ServerConfig::new()` which disables scripting. Elasticsearch tests validate
different REST API operations that benefit from action-based flexibility.

**Why no scripting?** Elasticsearch REST API is very diverse (CRUD, search, cluster ops) and benefits from testing each
operation separately. Scripting would reduce flexibility.

**Optimization Needed**: Tests should be consolidated to reduce LLM calls to <10:

- Test 1: Document operations (index, get, delete) - 4 calls
- Test 2: Search + bulk - 3 calls
- Test 3: Cluster operations + root - 3 calls
- **Target: 10 total LLM calls**

## Client Library

**reqwest** v0.12:

- Async HTTP client for tokio
- Handles HTTP/1.1 and HTTP/2
- JSON serialization/deserialization
- Simple and reliable

**No Official Client**: Intentionally avoided to keep tests simple

- Official client adds complexity
- Raw HTTP tests the protocol directly
- More control over request format

**Client Setup**:

```rust
let client = Client::new();
let url = format!("http://127.0.0.1:{}/products/_search", port);
let response = client
    .post(&url)
    .header("Content-Type", "application/json")
    .json(&json!({"query": {"match_all": {}}}))
    .send()
    .await?;
```

## Expected Runtime

**Model**: qwen3-coder:30b (default)
**Total Runtime**: ~70-100 seconds for all 7 tests
**Breakdown**:

- Each test: ~10-15 seconds (2 LLM calls)
- HTTP overhead minimal
- Variability: LLM response time

**Optimization**: Consolidating to 3 tests could reduce to ~30-40 seconds total.

## Failure Rate

**Historical**: ~5-8% failure rate
**Causes**:

1. **Invalid JSON**: LLM returns malformed JSON body
2. **Missing fields**: LLM forgets required response fields (e.g., `_index`, `hits`)
3. **Wrong status code**: LLM returns 500 instead of 200
4. **Empty response**: LLM returns no action

**Mitigation**:

- Flexible response validation (accept various valid formats)
- Explicit prompts for each operation type
- Retry helper for initial connection
- JSON validation in tests

## Test Cases

### 1. Search (`test_elasticsearch_search`)

**Validates**: Search API with query DSL

- Method: POST
- Path: `/products/_search`
- Body: `{"query": {"match_all": {}}}`
- Expected: 200 status with hits object
- Validates: Response is valid JSON with object structure
- **Expected LLM Response**: `elasticsearch_response` with hits array

### 2. Index Document (`test_elasticsearch_index_document`)

**Validates**: Index/create document

- Method: PUT
- Path: `/products/_doc/1`
- Body: `{"name": "Widget", "price": 19.99}`
- Expected: 200 status with `_index` or `result` field
- **Expected LLM Response**: `elasticsearch_response` with index result

### 3. Get Document (`test_elasticsearch_get_document`)

**Validates**: Retrieve document by ID

- Method: GET
- Path: `/products/_doc/123`
- Expected: 200 or 404 status (both valid)
- Validates: Response is valid JSON object
- **Expected LLM Response**: `elasticsearch_response` with document or not found

### 4. Bulk Operations (`test_elasticsearch_bulk_operations`)

**Validates**: Bulk API with multiple operations

- Method: POST
- Path: `/_bulk`
- Body: Newline-delimited JSON (NDJSON) format
- Expected: 200 status with `items` or `errors` field
- **Expected LLM Response**: `elasticsearch_response` with bulk result

### 5. Cluster Health (`test_elasticsearch_cluster_health`)

**Validates**: Cluster health endpoint

- Method: GET
- Path: `/_cluster/health`
- Expected: 200 status with health fields
- Validates: Response has `cluster_name`, `status`, or `acknowledged` field
- **Expected LLM Response**: `elasticsearch_response` with cluster health

### 6. Root Endpoint (`test_elasticsearch_root_endpoint`)

**Validates**: Cluster info at root path

- Method: GET
- Path: `/`
- Expected: 200 status
- Validates: `X-elastic-product: Elasticsearch` header present
- Validates: Response has cluster info fields
- **Expected LLM Response**: `elasticsearch_response` with cluster info

### 7. Delete Document (`test_elasticsearch_delete_document`)

**Validates**: Delete document by ID

- Method: DELETE
- Path: `/products/_doc/999`
- Expected: 200 status
- Validates: Response is valid JSON object
- **Expected LLM Response**: `elasticsearch_response` with deletion result

## Known Issues

### LLM Memory Limitations

**Issue**: LLM may forget previously indexed documents
**Symptom**: GET returns not found after index operation
**Workaround**: Tests don't rely on cross-test data persistence
**Status**: Each test is independent

### Flexible Response Validation

**Issue**: Elasticsearch responses can vary in structure
**Symptom**: Tests may fail on valid but unexpected response formats
**Workaround**: Tests validate minimal required fields only
**Status**: Tests are lenient and flexible

### Bulk Format Complexity

**Issue**: NDJSON format (newline-delimited JSON) is complex
**Symptom**: LLM may not understand bulk format correctly
**Workaround**: Tests verify bulk API accepts requests (not strict response validation)
**Status**: Works most of the time

### Connection Overhead

**Issue**: HTTP/1.1 without keep-alive creates new connection per request
**Symptom**: Slower than keep-alive would be
**Workaround**: Not a problem for tests
**Status**: By design for simplicity

## Test Execution

```bash
# Build release binary first (REQUIRED)
./cargo-isolated.sh build --release --all-features

# Run all Elasticsearch tests
./cargo-isolated.sh test --features elasticsearch --test server::elasticsearch::e2e_test

# Run specific test
./cargo-isolated.sh test --features elasticsearch --test server::elasticsearch::e2e_test test_elasticsearch_search

# Run with output
./cargo-isolated.sh test --features elasticsearch --test server::elasticsearch::e2e_test -- --nocapture
```

## Test Output Example

```
=== Test: Elasticsearch Search ===
Server started on port 54321 with stack: ETH>IP>TCP>HTTP>ELASTICSEARCH
[DEBUG] Search response: {"hits":{"total":{"value":2}}}
[PASS] Elasticsearch search request succeeded with valid JSON response
=== Test Complete ===
```

## Future Improvements

1. **Consolidation**: Merge tests to reduce LLM calls to 10
    - Test 1: Document CRUD (index, get, delete) - 4 calls
    - Test 2: Search + bulk - 3 calls
    - Test 3: Cluster + root - 3 calls
    - **Target: 10 total LLM calls**
2. **Scripting**: Enable scripting for repetitive operations
3. **Query DSL**: Test more complex queries (bool, match, term)
4. **Aggregations**: Test aggregation operations
5. **Index Management**: Test index settings, mappings
6. **Official Client**: Add optional tests using elasticsearch-rs (separate file)
