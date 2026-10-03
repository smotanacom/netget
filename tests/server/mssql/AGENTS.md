# MSSQL Server E2E Tests

## Overview

End-to-end tests for the MSSQL server implementation using the `tiberius` TDS client library. Tests validate that the NetGet MSSQL server correctly implements the TDS protocol and responds to SQL queries.

## Test Strategy

**Black-box testing**: Tests interact with the server through the TDS protocol using a real client (tiberius), validating protocol compliance and LLM response integration.

**Mock-based testing**: All LLM interactions are mocked to ensure deterministic test results and keep test execution fast. Tests should pass without requiring Ollama.

## Test Suite

### 1. `test_mssql_simple_query`
**Purpose**: Validate basic SELECT query with single-row, single-column result.

**Query**: `SELECT 1`

**Expected Response**:
- Columns: `[{"name": "result", "type": "INT"}]`
- Rows: `[[1]]`

**Mock LLM Calls**: 2
1. Server startup (`open_server` action)
2. Query response (`mssql_query_response` action)

**Runtime**: ~2-3 seconds

### 2. `test_mssql_multi_row_query`
**Purpose**: Validate multi-row result sets with multiple columns.

**Query**: `SELECT * FROM users`

**Expected Response**:
- Columns: `[{"name": "id", "type": "INT"}, {"name": "name", "type": "NVARCHAR"}]`
- Rows: `[[1, "Alice"], [2, "Bob"], [3, "Charlie"]]`

**Validation**: Ensures TDS COLMETADATA and ROW tokens are properly constructed.

**Mock LLM Calls**: 2
1. Server startup
2. Multi-row query response

**Runtime**: ~2-3 seconds

### 3. `test_mssql_create_table`
**Purpose**: Validate DDL (Data Definition Language) operations.

**Query**: `CREATE TABLE test (id INT PRIMARY KEY)`

**Expected Response**:
- Action: `mssql_ok_response`
- Rows affected: `1`

**Validation**: Ensures DONE token with rows_affected is sent correctly.

**Mock LLM Calls**: 2
1. Server startup
2. DDL response

**Runtime**: ~2-3 seconds

## LLM Call Budget

**Total LLM calls across all tests**: 6 (well under the 10-call limit)

**Breakdown**:
- Server startup: 3 calls (1 per test)
- Query responses: 3 calls (1 per test)

**Efficiency**: Each test spawns a new server instance but uses mock LLM responses for deterministic results.

## Test Execution

**Mock mode** (default - no Ollama required):
```bash
./test-e2e.sh mssql
```

**With real Ollama** (for validation):
```bash
./test-e2e.sh --use-ollama mssql
```

**Via cargo**:
```bash
./cargo-isolated.sh test --no-default-features --features mssql --test server::mssql::test
```

## Known Issues

### TDS Protocol Limitations

1. **No authentication**: Server accepts all connections without validating credentials. Tests use `AuthMethod::None`.

2. **Simplified type system**: All column types sent as NVARCHAR (0xE7) in TDS packets, despite type metadata. This works because tiberius can parse string representations of numbers.

3. **UTF-16LE encoding**: All strings encoded as UTF-16LE per TDS spec. Tests validate this encoding is correct.

### tiberius Client Quirks

1. **Connection timeout**: tiberius may take 2-3 seconds to establish connection due to TDS handshake. Tests use 10-second timeout.

2. **Query results API**: Results accessed via `into_results()` which returns `Vec<Vec<Row>>`. Tests extract first result set.

3. **Type extraction**: Use `row.get::<Type, _>(index)` with explicit type annotations. Returns `Option<T>`.

## Test Failures

### "Connection timeout"
**Cause**: Server not starting fast enough or TDS handshake failing.
**Fix**: Increase timeout in test or check server logs for TDS protocol errors.

### "No result received"
**Cause**: Mock LLM response not matching event trigger or action format incorrect.
**Fix**: Verify mock `.and_event_data_contains()` matches actual query string exactly.

### "Rows affected mismatch"
**Cause**: tiberius expects specific response format for DDL queries.
**Fix**: Ensure `mssql_ok_response` action includes `rows_affected` field.

## Protocol Compliance

Tests validate:
- ✅ TDS packet header format (8 bytes: type, status, length, SPID, packet_id, window)
- ✅ Pre-login response with version and encryption flags
- ✅ Login response with ENVCHANGE, INFO, and DONE tokens
- ✅ SQL Batch parsing (22-byte header + UTF-16LE SQL text)
- ✅ COLMETADATA token with column definitions
- ✅ ROW tokens with UTF-16LE encoded values
- ✅ DONE token with status and rows_affected

## Future Improvements

1. **Prepared statements**: Add tests for RPC packet type (currently returns error)
2. **Transactions**: Test BEGIN/COMMIT/ROLLBACK sequences
3. **Error handling**: Test ERROR token generation with specific error codes
4. **Large result sets**: Test pagination and streaming
5. **Binary types**: Test VARBINARY, IMAGE data types
6. **Date/time types**: Test DATETIME, DATE, TIME encoding
