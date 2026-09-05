# MySQL Protocol E2E Tests

## Test Overview

Tests MySQL server implementation using real `mysql_async` client library. Validates query execution, multi-row results,
DDL operations, the binary (prepared-statement) protocol, and rows whose value count disagrees with the column list.

## Test Strategy

**Consolidated Approach**: Multiple test functions but each tests a distinct MySQL feature (simple query, multi-row,
DDL). Each test spawns its own server with specific instructions.

**Why not more consolidated?** MySQL tests are already efficient - each test targets a different protocol feature and
completes quickly.

## LLM Call Budget

### Test: `test_mysql_simple_query`

- **1 server startup** (scripting disabled, action-based only)
- **1 SELECT 1 query** → LLM call for query response
- **Total: 2 LLM calls**

### Test: `test_mysql_multi_row_query`

- **1 server startup**
- **1 SELECT * FROM users query** → LLM call for multi-row response
- **Total: 2 LLM calls**

### Test: `test_mysql_create_table`

- **1 server startup**
- **1 CREATE TABLE query** → LLM call for DDL response
- **Total: 2 LLM calls**

### `prepared_statement_test.rs`

Three tests, each **1 server startup + 2-3 query calls** (mysql_async issues one `SELECT @@*`
during handshake, then the statement). `placeholder_counting_skips_literals_and_comments` is a
pure unit test and makes **0** LLM calls.

### `llm_failure_test.rs` / `connection_stats_test.rs`

`connection_stats_test` runs with a static handler and makes **0** LLM calls, which is what
makes it a counter test rather than a timing test.

**Total for the MySQL suite: ~17 LLM calls across 9 tests**, each test on its own server.

## Scripting Usage

**Scripting Disabled**: All tests use `ServerConfig::new()` which disables scripting by default. MySQL tests rely on
action-based responses for flexibility in testing different query patterns.

**Why no scripting?** MySQL queries are highly variable (SELECT vs DDL vs DML), making scripting less practical.
Action-based responses provide better test coverage.

## Client Library

**mysql_async** v0.34:

- Full-featured async MySQL client using tokio
- Supports both simple and prepared statements
- Handles connection handshake and authentication
- Provides typed result extraction
- Used for protocol correctness validation

**Client Setup**:

```rust
let opts = mysql_async::OptsBuilder::default()
    .ip_or_hostname("127.0.0.1")
    .tcp_port(port)
    .user(Some("root"))
    .pass(Some(""));
let pool = mysql_async::Pool::new(opts);
let mut conn = pool.get_conn().await?;
```

## Expected Runtime

**Model**: qwen3-coder:30b (default)
**Total Runtime**: ~40-60 seconds for all 3 tests
**Breakdown**:

- Each test: ~15-20 seconds (1 startup + 1 query call)
- Variability: LLM response time, network latency

**Optimization**: Tests could be parallelized but already fast enough individually.

## Failure Rate

**Historical**: ~5% failure rate
**Causes**:

1. **Connection timeout**: Client timeout (10s) expires before LLM responds
2. **Type mismatch**: LLM returns wrong column type (e.g., VARCHAR instead of INT)
3. **Empty result**: LLM forgets to include query_response action
4. **Variable queries**: `SELECT @@*` system variable queries may confuse LLM

**Mitigation**:

- Explicit prompts for system variable queries
- Timeout extended to 10s (may need longer for slow models)
- Test retries on timeout failures

## Test Cases

### 1. Simple Query (`test_mysql_simple_query`)

**Validates**: Basic SELECT query with single row

- Connects to MySQL server
- Executes `SELECT 1`
- Verifies result is integer `1`
- **Expected LLM Response**: `mysql_query_response` with INT column

### 2. Multi-Row Query (`test_mysql_multi_row_query`)

**Validates**: SELECT query returning multiple rows

- Executes `SELECT * FROM users`
- Expects 3 rows: Alice, Bob, Charlie
- Verifies row count and data structure
- **Expected LLM Response**: `mysql_query_response` with 3-row array

### 3. CREATE TABLE (`test_mysql_create_table`)

**Validates**: DDL operation handling

- Executes `CREATE TABLE test (id INT PRIMARY KEY)`
- Expects success or non-fatal error
- Tests server doesn't crash on DDL
- **Expected LLM Response**: `mysql_ok` with affected_rows=1

## Known Issues

### System Variable Queries

**Issue**: `SELECT @@version_comment`, `SELECT @@max_allowed_packet` etc.
**Symptom**: mysql_async client sends these during connection setup, LLM may not handle them
**Workaround**: Prompt explicitly instructs LLM to return `mysql_query_response` for `SELECT @@*` queries
**Example Fix**:
`For SELECT @@* queries, return mysql_query_response columns=[{name:'value',type:'VARCHAR'}] rows=[['1000']]`

### Connection Timeout

**Issue**: LLM takes >10s to respond, client times out
**Symptom**: "Connection timeout" error
**Workaround**: Consider increasing timeout to 30s for slow models
**Not Flaky**: Consistent on slow hardware/models

### Type Precision

**Issue**: LLM may return string `"1"` where the column is declared `INT`.
**Status**: Handled. `write_cell` coerces the JSON value to the declared column type (a
numeric string parses), and a value that genuinely cannot be represented is sent as **NULL**
with a WARN rather than as a wrong number or as an error that ends the session.

**This used to say "implementation converts JSON to strings; client parses strings to expected
types", and that was the bug, not the workaround.** It is true of the *text* protocol only. In
the binary protocol (`conn.exec*`) opensrv-mysql encodes by `Column::coltype` and rejects a
string for any numeric or temporal column with an `io::Error` that ends the session — so the
protocol's own advertised example, `{"name": "id", "type": "INT"}`, killed the connection on
every prepared statement. `prepared_statement_test.rs` is the regression test.

## Test Execution

```bash
# Run all MySQL server tests. `--test` names a test *target*; `server` is the target and the
# filter goes after `--`. `--test server::mysql::test` makes cargo list targets and exit.
./cargo-isolated.sh test --no-default-features --features mysql \
    --test server -- server::mysql --test-threads=100

# Run one test
./cargo-isolated.sh test --no-default-features --features mysql \
    --test server -- server::mysql::test::test_mysql_simple_query

# Run with output
./cargo-isolated.sh test --no-default-features --features mysql \
    --test server -- server::mysql --nocapture
```

## Test Output Example

```
=== E2E Test: MySQL Simple Query ===
Server started on port 54321
Connecting to MySQL server...
✓ MySQL connected
Executing SELECT 1...
✓ Received correct result: 1
✓ MySQL simple query test passed
```

## Future Improvements

1. **Transactions**: Test BEGIN/COMMIT/ROLLBACK sequences
2. **Consolidation**: Merge tests into a single server with multiple queries

Done, and recorded so they are not re-listed: prepared statements
(`prepared_statement_test.rs` covers PREPARE with and without a bound parameter, and the
binary result-set encoding for INT/BIGINT/DOUBLE/VARCHAR/TEXT plus SQL NULL) and
LLM-generated error responses (`llm_failure_test.rs`; opensrv-mysql has had
`QueryResultWriter::error` since 0.4, so "once opensrv supports errors" was never true).
