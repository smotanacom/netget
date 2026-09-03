# Cassandra Client E2E Tests

## Overview

E2E tests for the Cassandra client implementation. Tests verify client functionality using the NetGet binary in
black-box mode.

## Test Strategy

**Approach**: Black-box E2E testing

- Start NetGet server (Cassandra protocol)
- Start NetGet client connecting to server
- Verify LLM-controlled query execution
- Validate client-server interaction

**No Docker**: Tests use NetGet's built-in Cassandra server (not external Docker container)

## Test Suite

### Test 1: Basic Connection and Query

**Function**: `test_cassandra_client_connect_and_query`

**LLM Calls**: 3

1. Server startup (parse "Cassandra" from instruction)
2. Client connection (parse protocol, connect)
3. Query execution (execute SELECT query)

**Runtime**: ~2-3 seconds

**What it tests**:

- Client connects to Cassandra server
- Client executes basic CQL query
- Results received and processed

**Instruction**:

- Server: "Listen on port {AVAILABLE_PORT} via Cassandra. Accept CQL queries. For SELECT * FROM system.local, return a
  result set with host_id and cluster_name columns."
- Client: "Connect to 127.0.0.1:{port} via Cassandra. Execute 'SELECT * FROM system.local' query."

### Test 2: Query with Consistency Level

**Function**: `test_cassandra_client_with_consistency`

**LLM Calls**: 3

1. Server startup
2. Client connection
3. Query execution with consistency parameter

**Runtime**: ~2-3 seconds

**What it tests**:

- Client specifies consistency level in query
- LLM interprets consistency requirement from instruction
- Server logs consistency level

**Instruction**:

- Server: "Listen on port {AVAILABLE_PORT} via Cassandra. Accept CQL queries and log consistency levels."
- Client: "Connect to 127.0.0.1:{port} via Cassandra. Execute 'SELECT * FROM system.local' with QUORUM consistency."

### Test 3: Multi-Step Query Execution

**Function**: `test_cassandra_client_multi_query`

**LLM Calls**: 4+

1. Server startup
2. Client connection
3. First query execution
4. Second query execution (after first completes)

**Runtime**: ~3-4 seconds

**What it tests**:

- Client executes multiple queries sequentially
- LLM processes results and decides next action
- State machine transitions (Idle → Processing → Idle)

**Instruction**:

- Server: "Listen on port {AVAILABLE_PORT} via Cassandra. Accept CQL queries. For SELECT queries, return mock results."
- Client: "Connect to 127.0.0.1:{port} via Cassandra. First, query system.local. Then query system.peers."

## LLM Call Budget

**Total LLM Calls**: 10

- Test 1: 3 calls
- Test 2: 3 calls
- Test 3: 4 calls

**Budget Compliance**: ✅ < 10 calls per test (within budget)

**Total Suite LLM Calls**: 10 calls (within recommended < 30 for suite)

## Running Tests

```bash
# Single test
./cargo-isolated.sh test --no-default-features --features cassandra --test client::cassandra::e2e_test -- test_cassandra_client_connect_and_query

# Full suite (recommended)
./cargo-isolated.sh test --no-default-features --features cassandra --test client::cassandra::e2e_test

# With output
./cargo-isolated.sh test --no-default-features --features cassandra --test client::cassandra::e2e_test -- --nocapture
```

**Build Time**: ~15-30s (cassandra feature only)
**Test Runtime**: ~10-15s total

## Expected Runtime

- **Build** (first time): 30s
- **Build** (cached): 5s
- **Test execution**: 10s
- **Total**: ~15-40s depending on cache

## Test Infrastructure

**Uses**: `tests/helpers.rs`

- `start_netget_server()`: Spawn NetGet server process
- `start_netget_client()`: Spawn NetGet client process
- `NetGetConfig`: Test configuration builder
- Port management: `{AVAILABLE_PORT}` placeholder

**Assertions**:

- `output_contains("connected")`: Verify connection message
- `assert_eq!(client.protocol, "Cassandra")`: Verify protocol detection

## Known Issues

1. **Timing Sensitivity**: Tests use `tokio::time::sleep()` for coordination
    - 1000ms delays for Cassandra connection (longer than Redis due to protocol handshake)
    - May need adjustment on slow CI systems

2. **Result Parsing Simplified**: Tests don't validate actual row data
    - Only checks connection and execution
    - Future: Parse result JSON and validate structure

3. **No Real Cassandra Server**: Tests use NetGet's mock server
    - Good: Fast, no Docker dependencies
    - Bad: May not catch real protocol issues

## Future Enhancements

- **Docker Tests**: Optional real Cassandra container tests
- **Result Validation**: Parse and validate row data structure
- **Prepared Statements**: Test prepared statement caching
- **Error Cases**: Test query errors, connection failures
- **Batch Operations**: Test batch query execution
- **Authentication**: Test username/password authentication

## Debugging Failed Tests

**If test fails with "Client should show connection message"**:

1. Check `client.get_output()` for actual output
2. Verify port is available (not blocked by firewall)
3. Increase sleep duration if timing issue
4. Check netget.log for detailed client/server logs

**If test fails with "Unknown Cassandra client action"**:

1. Check LLM generated valid action JSON
2. Verify action type matches `execute_action()` implementation
3. Check netget.log for LLM response

**If test times out**:

1. Verify Ollama is running and accessible
2. Check the mock LLM rules actually match (an unmatched rule falls through to a real call)
3. Increase test timeout if LLM is slow

## References

- Main tests: `tests/client/cassandra/e2e_test.rs`
- Test helpers: `tests/helpers.rs`
- Client implementation: `src/client/cassandra/`
- Test infrastructure docs: `TEST_INFRASTRUCTURE_FIXES.md`
