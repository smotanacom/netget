# PostgreSQL Client E2E Tests

## Test Strategy

Black-box E2E tests using NetGet binary. Tests verify PostgreSQL client functionality including:

- Connection establishment
- Query execution
- Transaction support
- LLM-controlled actions

## LLM Call Budget

**Target:** < 10 calls
**Actual:** 6 calls, all in `a_query_result_drives_exactly_one_more_query`.

**The other three tests are `#[ignore]`d** — they configure no `.with_mock()` and so need
`--use-ollama`. Until `a_query_result_drives_exactly_one_more_query` was added, this client's
entire model-driven path ran in no test at all; only the zero-LLM command-channel test
executed. Read this budget as describing one running test, not three.

- test_postgresql_client_connect_and_query: 2 calls (server + client)
- test_postgresql_client_llm_controlled_queries: 2 calls (server + client)
- test_postgresql_client_transactions: 2 calls (server + client)

## Test Server Setup

```bash
# Start PostgreSQL with Docker (optional, for testing against real server)
docker run -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:latest

# Run tests against NetGet PostgreSQL server
./cargo-isolated.sh test --no-default-features --features postgresql --test client::postgresql::e2e_test
```

## Tests

### 1. test_postgresql_client_connect_and_query (2 LLM calls)

- Start NetGet PostgreSQL server
- Connect PostgreSQL client to server
- Execute simple SELECT query
- Verify connection and query execution
- **Validates:** Basic connectivity, query execution, result handling

### 2. test_postgresql_client_llm_controlled_queries (2 LLM calls)

- Start NetGet PostgreSQL server
- Client sends LLM-instructed query
- Verify protocol detection
- **Validates:** LLM control, query generation, protocol identification

### 3. test_postgresql_client_transactions (2 LLM calls)

- Start NetGet PostgreSQL server
- Client executes BEGIN/COMMIT transaction
- Verify transaction control
- **Validates:** Transaction management, multi-statement execution

## Runtime

**Expected:** < 10 seconds
**Actual:** ~2-3 seconds with Ollama caching

## Known Issues

- **TLS Not Implemented** - Client uses NoTls, should add rustls support
- **Limited Type Support** - All values converted to strings
- **No Prepared Statements** - Simple query protocol only
- **No Connection Pooling** - Single connection per client

## Future Tests

- Integration test with real PostgreSQL Docker container
- Test complex queries with JOINs
- Test prepared statements
- Test LISTEN/NOTIFY pub/sub
- Test COPY protocol
- Test connection parameters (database, user, password)
- Test error handling (syntax errors, constraint violations)
- Test concurrent query execution
