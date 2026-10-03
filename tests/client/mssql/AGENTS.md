# MSSQL Client E2E Tests

## Overview

End-to-end tests for the MSSQL client implementation. Tests validate that the NetGet MSSQL client can connect to MSSQL servers and execute queries under LLM control.

## Test Strategy

**Black-box testing**: Tests spawn both an MSSQL server (NetGet) and an MSSQL client (NetGet) and validate their interaction through mock LLM responses.

**Mock-based testing**: All LLM interactions are mocked for deterministic results and fast execution. Tests should pass without requiring Ollama.

## Test Suite

### 1. `test_mssql_client_connect_and_query_with_mocks`
**Purpose**: Validate basic client connection and simple query execution.

**Flow**:
1. Start NetGet MSSQL server on random port
2. Start NetGet MSSQL client connecting to server
3. Client executes `SELECT 1` query via LLM action
4. Server responds with result set
5. Client receives query result event

**Mock LLM Calls**: 6
1. Server startup (`open_server`)
2. Server query response (`mssql_query_response`)
3. Client startup (`open_client`)
4. Client connection (`execute_query` on `mssql_connected` event)
5. Client query result (`wait_for_more` on `mssql_query_result` event)

**Validation**:
- Client output contains "connected"
- Mock expectations verified for both server and client

**Runtime**: ~3-4 seconds

### 2. `test_mssql_client_multi_row_query_with_mocks`
**Purpose**: Validate multi-row query results handled correctly by client.

**Flow**:
1. Start NetGet MSSQL server
2. Server configured to return 3 rows for `SELECT * FROM users`
3. Client connects and executes query via LLM
4. Client receives multi-row result set

**Query**: `SELECT * FROM users`

**Expected Response**:
- Columns: `[{"name": "id", "type": "INT"}, {"name": "name", "type": "NVARCHAR"}]`
- Rows: `[[1, "Alice"], [2, "Bob"], [3, "Charlie"]]`

**Mock LLM Calls**: 6
1. Server startup
2. Server multi-row response
3. Client startup
4. Client connection
5. Client query result (validated with `.and_event_data_contains("rows", "Alice")`)
6. (Implicit wait)

**Runtime**: ~3-4 seconds

## LLM Call Budget

**Total LLM calls across all tests**: 12

**Breakdown**:
- Server startup: 2 calls (1 per test)
- Server query responses: 2 calls (1 per test)
- Client startup: 2 calls (1 per test)
- Client connection events: 2 calls (1 per test)
- Client query result events: 2 calls (1 per test)
- Wait actions: 2 calls (implicit in mocks)

**Efficiency**: Well under the 10-call guideline per individual test. Total across suite is acceptable for comprehensive testing.

## Test Execution

**Mock mode** (default - no Ollama required):
```bash
./test-e2e.sh mssql-client
```

**With real Ollama** (for validation):
```bash
./test-e2e.sh --use-ollama mssql-client
```

**Via cargo**:
```bash
./cargo-isolated.sh test --no-default-features --features mssql --test client::mssql::e2e_test
```

## Known Issues

### Client-Server Coordination

1. **Timing sensitivity**: Tests use `tokio::time::sleep()` to allow server/client startup. May need adjustment on slower systems.

2. **Connection timeout**: Client may timeout if server not ready. Tests use 1-second wait between server start and client start.

### tiberius Client Library

1. **Connection string parsing**: Current implementation parses `host:port;database=db;user=user` format. Tests use simple `host:port`.

2. **Authentication**: Tests use `AuthMethod::None` which works with our no-auth server implementation.

3. **Query execution**: Client uses `client.query(sql, &[])` which returns `QueryStream`. Results extracted via `into_results()`.

### Mock Validation

1. **Event data matching**: `.and_event_data_contains("query", "SELECT 1")` requires exact substring match. Case-sensitive.

2. **Action format**: Mock actions must match exact JSON structure expected by client action parser.

## Test Failures

### "Client should show connection message"
**Cause**: Client not connecting successfully or output not captured.
**Fix**: Increase sleep duration, check server startup logs, verify mock actions.

### "Mock expectation not met"
**Cause**: Expected LLM call not made or made with different parameters.
**Fix**: Check event type, event data, and action format in mocks.

### "Connection timeout"
**Cause**: Server not accepting connections or TDS handshake failing.
**Fix**: Verify server is running, check server logs for TDS protocol errors.

## Protocol Validation

Client tests indirectly validate:
- ✅ TDS client connection handshake
- ✅ SQL Batch query execution
- ✅ Result set parsing (columns + rows)
- ✅ Multi-row result handling
- ✅ LLM event triggering on connection and query results
- ✅ Action execution (execute_query, wait_for_more)

## Future Improvements

1. **Error handling**: Test client receiving ERROR token from server
2. **DDL queries**: Test CREATE/INSERT/UPDATE with `rows_affected`
3. **Connection string parsing**: Test database and user parameters
4. **Reconnection**: Test client reconnecting after disconnect
5. **Large result sets**: Test pagination and streaming
6. **Concurrent queries**: Test multiple queries in sequence
