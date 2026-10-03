# DynamoDB Protocol E2E Tests

## Test Overview

Tests DynamoDB-compatible server implementation using HTTP client (`reqwest`). Validates CRUD operations (GetItem,
PutItem, Query, DeleteItem) and table management (CreateTable). Does NOT use AWS SDK to avoid authentication complexity.

## Test Strategy

**HTTP-Based Testing**: Uses raw HTTP client instead of AWS SDK. Sends POST requests with `x-amz-target` header and JSON
bodies. This approach:

- Avoids AWS authentication/signature complexity
- Tests HTTP protocol directly
- Validates JSON request/response format
- Simpler and more maintainable

**Consolidated Operations**: Multiple small tests, each testing one operation. Could be further consolidated.

## LLM Call Budget

### Test: `test_dynamo_get_item`

- **1 server startup**
- **1 GetItem request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_dynamo_put_item`

- **1 server startup**
- **1 PutItem request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_dynamo_query`

- **1 server startup**
- **1 Query request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_dynamo_create_table`

- **1 server startup**
- **1 CreateTable request** → LLM call
- **Total: 2 LLM calls**

### Test: `test_dynamo_multiple_operations`

- **1 server startup**
- **3 operations** (PutItem, GetItem, DeleteItem) → 3 LLM calls
- **Total: 4 LLM calls**

**Total for DynamoDB test suite: 12 LLM calls** (slightly over 10 limit)

## Scripting Usage

**Scripting Disabled**: All tests use `ServerConfig::new()` which disables scripting. DynamoDB tests validate different
operations that benefit from action-based flexibility.

**Why no scripting?** DynamoDB operations are diverse (CRUD, table management) and benefit from testing each separately.
Scripting would reduce flexibility.

**Optimization**: Tests could be consolidated into 2-3 comprehensive tests with scripting to reduce LLM calls to <10.

## Client Library

**reqwest** v0.12:

- Async HTTP client for tokio
- Handles HTTP/1.1 and HTTP/2
- JSON serialization/deserialization
- Simple and reliable

**No AWS SDK**: Intentionally avoided to keep tests simple

- AWS SDK requires authentication/credentials
- SDK adds complexity and dependencies
- Raw HTTP tests the protocol directly

**Client Setup**:

```rust
let client = Client::new();
let url = format!("http://127.0.0.1:{}", port);
let response = client
    .post(&url)
    .header("x-amz-target", "DynamoDB_20120810.GetItem")
    .header("Content-Type", "application/x-amz-json-1.0")
    .json(&json!({
        "TableName": "Users",
        "Key": {"id": {"S": "user-123"}}
    }))
    .send()
    .await?;
```

## Expected Runtime

**Model**: qwen3-coder:30b (default)
**Total Runtime**: ~50-70 seconds for all 5 tests
**Breakdown**:

- Each test: ~10-15 seconds (2-4 LLM calls)
- HTTP overhead minimal
- Variability: LLM response time

**Optimization**: Tests could be consolidated to ~30-40 seconds total.

## Failure Rate

**Historical**: ~5% failure rate
**Causes**:

1. **Invalid JSON**: LLM returns malformed JSON body
2. **Missing fields**: LLM forgets required response fields
3. **Wrong status code**: LLM returns 500 instead of 200
4. **Empty response**: LLM returns no action

**Mitigation**:

- Explicit prompts for each operation type
- Retry helper for initial connection
- JSON validation in tests
- Flexible response validation (accept various valid formats)

## Test Cases

### 1. GetItem (`test_dynamo_get_item`)

**Validates**: Retrieve item by key

- Operation: `DynamoDB_20120810.GetItem`
- Request: TableName="Users", Key={id: "user-123"}
- Expected: 200 status with Item object
- **Expected LLM Response**: `dynamo_response` with Item JSON

### 2. PutItem (`test_dynamo_put_item`)

**Validates**: Insert/update item

- Operation: `DynamoDB_20120810.PutItem`
- Request: TableName="Users", Item={id, name, email}
- Expected: 200 status
- **Expected LLM Response**: `dynamo_response` with empty body or attributes

### 3. Query (`test_dynamo_query`)

**Validates**: Query items with key condition

- Operation: `DynamoDB_20120810.Query`
- Request: TableName="Users", KeyConditionExpression
- Expected: 200 status with Items array
- Validates: Response is valid JSON
- **Expected LLM Response**: `dynamo_response` with Items/Count

### 4. CreateTable (`test_dynamo_create_table`)

**Validates**: Table creation

- Operation: `DynamoDB_20120810.CreateTable`
- Request: TableName="Products", KeySchema, AttributeDefinitions
- Expected: 200 status
- **Expected LLM Response**: `dynamo_response` with TableDescription or empty

### 5. Multiple Operations (`test_dynamo_multiple_operations`)

**Validates**: Sequential operations with LLM "memory"

- PutItem: Insert order-001
- GetItem: Retrieve order-001 (LLM should "remember")
- DeleteItem: Delete order-001
- All operations should succeed
- **Expected**: LLM maintains virtual data across operations

## Known Issues

### LLM Memory Limitations

**Issue**: LLM may forget previously inserted data
**Symptom**: GetItem returns empty after PutItem
**Workaround**: Prompt emphasizes "remember items across requests"
**Status**: Works most of the time, occasional failures

### JSON Format Flexibility

**Issue**: DynamoDB JSON format is complex (type descriptors: S, N, etc.)
**Symptom**: LLM may return simplified JSON without type descriptors
**Workaround**: Tests accept flexible response formats
**Status**: Not a blocker, tests are lenient

### Request ID Generation

**Issue**: Timestamp-based IDs may collide
**Symptom**: Duplicate request IDs (cosmetic only)
**Workaround**: Use nanosecond precision
**Status**: Extremely rare

### Connection Overhead

**Issue**: HTTP/1.1 without keep-alive creates new connection per request
**Symptom**: Slower than keep-alive would be
**Workaround**: Not a problem for tests
**Status**: By design for simplicity

## Test Execution

```bash
# Run all DynamoDB tests
./cargo-isolated.sh test --features dynamo --test server -- server::dynamo

# Run specific test
./cargo-isolated.sh test --features dynamo --test server -- server::dynamo::e2e_test::test_dynamo_get_item

# Run with output
./cargo-isolated.sh test --features dynamo --test server -- server::dynamo::e2e_test --nocapture
```

## Test Output Example

```
=== Test: DynamoDB GetItem ===
Server started on port 54321 with stack: ETH>IP>TCP>HTTP>DYNAMO
[PASS] DynamoDB GetItem request succeeded
=== Test Complete ===
```

## Future Improvements

1. **Consolidation**: Merge tests to reduce LLM calls to <10
    - Test 1: Basic CRUD (GetItem, PutItem, DeleteItem) - 4 calls
    - Test 2: Query + CreateTable - 3 calls
    - **Target: 7-8 total LLM calls**
2. **Scripting**: Enable scripting for repetitive operations
3. **BatchOperations**: Test BatchGetItem, BatchWriteItem
4. **UpdateItem**: Test UpdateExpression syntax
5. **Conditional Operations**: Test ConditionExpression
6. **AWS SDK Tests**: Add optional tests using official SDK (separate file)
