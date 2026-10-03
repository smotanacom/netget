# Cassandra/CQL Protocol E2E Tests

## Test Overview

Tests Cassandra/CQL server implementation using real `scylla` (ScyllaDB/Cassandra driver) client library. Validates
connection startup, simple queries, prepared statements, error handling, and concurrent connections.

## Test Strategy

**Comprehensive Multi-Phase Testing**: Tests are organized into multiple functions covering:

- Phase 1: Basic connection and queries
- Phase 2: Prepared statements (single and multiple)
- Phase 3: Error handling and edge cases
- Concurrent operations

Each test spawns its own server with specific instructions for that scenario.

## LLM Call Budget

### Test: `test_cassandra_connection`

- **1 server startup**
- **Connection only** (OPTIONS/STARTUP handled by client, no custom queries)
- **Total: 1 LLM call**

### Test: `test_cassandra_select_query`

- **1 server startup**
- **1 SELECT query** → LLM call
- **Total: 2 LLM calls**

### Test: `test_cassandra_error_response`

- **1 server startup**
- **1 error query** → LLM call
- **Total: 2 LLM calls**

### Test: `test_cassandra_multiple_queries`

- **1 server startup**
- **2 queries** → 2 LLM calls
- **Total: 3 LLM calls**

### Test: `test_cassandra_concurrent_connections`

- **1 server startup**
- **3 concurrent queries** → 3 LLM calls
- **Total: 4 LLM calls**

### Test: `test_cassandra_prepared_statement`

- **1 server startup**
- **1 PREPARE** → LLM call
- **1 EXECUTE** → LLM call
- **Total: 3 LLM calls**

### Test: `test_cassandra_multiple_prepared_statements`

- **1 server startup**
- **2 PREPARE** → 2 LLM calls
- **3 EXECUTE** → 3 LLM calls
- **Total: 6 LLM calls**

### Test: `test_cassandra_prepared_statement_param_mismatch`

- **1 server startup**
- **1 PREPARE** → LLM call
- **1 EXECUTE (error)** → LLM call
- **Total: 3 LLM calls**

**Total for Cassandra test suite: 24 LLM calls** (exceeds 10 limit significantly)

## Scripting Usage

**Scripting Disabled**: All tests use `ServerConfig::new()` which disables scripting by default. Cassandra tests
validate complex protocol flows (prepared statements, multiple queries) that benefit from action-based flexibility.

**Why no scripting?** Cassandra protocol has complex state (prepared statements, parameter binding) that is difficult to
script. Action-based responses provide necessary flexibility.

**Optimization Needed**: Test suite should be consolidated to reduce LLM calls below 10 limit. Suggested approach:

- Consolidate basic tests into 1-2 comprehensive tests
- Use scripting for repetitive queries
- Target: 8-10 total LLM calls

## Client Library

**scylla** v0.13:

- Full-featured async Cassandra/ScyllaDB client using tokio
- Supports CQL native protocol v4
- Handles connection pooling, prepared statements, retries
- Provides `SessionBuilder` for connection setup
- Query execution: `query_unpaged()`, `execute_unpaged()`
- Used for protocol correctness validation

**Client Setup**:

```rust
let uri = format!("127.0.0.1:{}", port);
let session: Session = SessionBuilder::new()
    .known_node(&uri)
    .build()
    .await
    .expect("Failed to connect");
```

## Expected Runtime

**Model**: qwen3-coder:30b (default)
**Total Runtime**: ~3-5 minutes for all 8 tests
**Breakdown**:

- Connection test: ~10s (1 call)
- Simple tests: ~15-20s each (2-3 calls)
- Prepared statement tests: ~25-35s each (3-6 calls)
- Concurrent test: ~30s (4 calls)
- Variability: LLM response time, protocol complexity

**Optimization**: Consolidating tests could reduce runtime to ~1-2 minutes.

## Failure Rate

**Historical**: ~10% failure rate
**Causes**:

1. **Connection timeout**: LLM slow on STARTUP/OPTIONS
2. **Frame parsing errors**: LLM returns malformed response
3. **Prepared statement ID mismatch**: Hash collision (very rare)
4. **Type errors**: LLM returns wrong column type
5. **Empty result**: LLM forgets action

**Mitigation**:

- Explicit prompts for protocol operations
- 2-second sleep after server startup (gives LLM time)
- Clear instructions for prepared statements
- Error handling tests validate error responses

## Test Cases

### 1. Connection (`test_cassandra_connection`)

**Validates**: Basic connection, STARTUP, OPTIONS, READY flow

- Connects to Cassandra server
- Verifies handshake completes
- No queries executed
- **Expected**: Server accepts connection and sends READY

### 2. SELECT Query (`test_cassandra_select_query`)

**Validates**: Simple CQL query with result set

- Executes `SELECT * FROM users`
- Expects 3 rows: Alice, Bob, Charlie
- Verifies row count > 0
- **Expected LLM Response**: `cassandra_result_rows` with 3 rows

### 3. Error Response (`test_cassandra_error_response`)

**Validates**: Error handling with proper error codes

- Executes `SELECT * FROM nonexistent`
- Expects error response
- Verifies error code 0x2200 (table does not exist)
- **Expected LLM Response**: `cassandra_error` with code/message

### 4. Multiple Queries (`test_cassandra_multiple_queries`)

**Validates**: Sequential query execution on same connection

- Executes `SELECT count(*) FROM users`
- Executes `SELECT * FROM users WHERE id=1`
- Both queries should succeed
- **Expected LLM Responses**: Two separate `cassandra_result_rows` actions

### 5. Concurrent Connections (`test_cassandra_concurrent_connections`)

**Validates**: Multiple simultaneous connections

- Spawns 3 concurrent connections
- Each executes a SELECT query
- All should complete successfully
- **Expected**: Server handles concurrent connections correctly

### 6. Prepared Statement (`test_cassandra_prepared_statement`)

**Validates**: PREPARE/EXECUTE flow with parameters

- PREPARE: `SELECT * FROM users WHERE id = ?`
- EXECUTE with parameter: 1
- Expects result with id=1, name='Alice'
- **Expected LLM Responses**:
    - PREPARE: `cassandra_prepared` with metadata
    - EXECUTE: `cassandra_result_rows` with 1 row

### 7. Multiple Prepared Statements (`test_cassandra_multiple_prepared_statements`)

**Validates**: Multiple prepared statements and reuse

- Prepare statement 1: `SELECT * FROM users WHERE id = ?`
- Prepare statement 2: `SELECT count(*) FROM users`
- Execute statement 1 with param 1
- Execute statement 2
- Execute statement 1 again with param 2
- **Expected**: Both statements work independently

### 8. Parameter Mismatch (`test_cassandra_prepared_statement_param_mismatch`)

**Validates**: Parameter validation in EXECUTE

- Prepare statement with 2 parameters
- Try to execute with only 1 parameter
- Expects error
- **Expected LLM Response**: `cassandra_error` with code 0x2200

## Known Issues

### Connection Timeout

**Issue**: First connection may timeout if LLM is slow on STARTUP
**Symptom**: "Failed to connect to Cassandra" error
**Workaround**: 2-second sleep after server startup
**Status**: Rare, only on very slow models

### Prepared Statement ID Collisions

**Issue**: Hash-based statement IDs may collide (extremely rare)
**Symptom**: Wrong query executed
**Workaround**: Use distinct query strings in tests
**Status**: Never observed in practice

### Type Mapping

**Issue**: Limited type support (int, varchar, boolean only in Phase 1)
**Symptom**: Other types not recognized
**Workaround**: Tests only use supported types
**Status**: By design for Phase 1

### Frame Size

**Issue**: Very large result sets may exceed frame size
**Symptom**: Protocol error or truncation
**Workaround**: Tests use small result sets
**Status**: Not a problem for test data

## Test Execution

```bash
# Build release binary first (REQUIRED)
./cargo-isolated.sh build --release --all-features

# Run all Cassandra tests
./cargo-isolated.sh test --features cassandra --test server::cassandra::e2e_test

# Run specific test
./cargo-isolated.sh test --features cassandra --test server::cassandra::e2e_test test_cassandra_connection

# Run with output
./cargo-isolated.sh test --features cassandra --test server::cassandra::e2e_test -- --nocapture
```

## Test Output Example

```
=== Test: Cassandra Connection ===
  [TEST] Connecting to 127.0.0.1:9042
  [TEST] ✓ Connection successful
  [TEST] ✓ Test completed successfully
```

## Future Improvements

1. **Consolidation**: Merge tests to reduce LLM calls to <10
    - Test 1: Connection + simple query (2 calls)
    - Test 2: Multiple queries + error handling (3 calls)
    - Test 3: Prepared statements (3-4 calls)
    - **Target: 8-9 total LLM calls**
2. **Scripting**: Enable scripting for repetitive queries
3. **More Types**: Test collections, UDTs once implemented
4. **Batching**: Test BATCH operations
5. **Paging**: Test large result sets with paging
6. **Authentication**: Test SASL authentication (Phase 3)
