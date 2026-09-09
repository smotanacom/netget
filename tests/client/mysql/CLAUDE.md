# MySQL Client E2E Tests

## Overview

End-to-end tests for the MySQL client implementation. These tests verify that the MySQL client can connect to MySQL
servers (NetGet or real MySQL), execute queries, and handle transactions under LLM control.

## Test Strategy

**Approach:** Black-box testing using NetGet's own MySQL server as the test target. The tests spawn two NetGet
instances:

1. **Server instance:** MySQL server listening on a random port
2. **Client instance:** MySQL client connecting to that server

This approach ensures:

- Real protocol interaction
- LLM integration testing
- Client-server compatibility

**Alternative:** Tests could connect to a real MySQL Docker container for full protocol compliance testing.

## Test Suite

### Test 1: `test_mysql_client_connect_and_query`

**Purpose:** Verify basic connection and simple query execution

**LLM Calls:** 2

1. Server startup (parse instruction, start MySQL server)
2. Client connection (parse instruction, connect, execute query)

**Flow:**

1. Start MySQL server on random port
2. Start MySQL client with connection instruction
3. Client executes `SELECT 1` query
4. Verify connection message in output
5. Cleanup

**Expected Behavior:**

- Client shows "connected" message
- Query executes successfully
- No errors in output

**Runtime:** ~1-2 seconds

---

### Test 2: `test_mysql_client_with_database`

**Purpose:** Test database selection via startup parameters

**LLM Calls:** 2

1. Server startup
2. Client connection with database specified

**Flow:**

1. Start MySQL server accepting 'testdb' database
2. Client connects with database='testdb' parameter
3. Client executes `SELECT * FROM users`
4. Verify protocol is "MySQL"
5. Cleanup

**Expected Behavior:**

- Client connects to specific database
- **NOT covered: startup params.** `test_mysql_client_with_database` puts `username` and
  `database` at the *top level* of the `open_client` action, but `CommonAction::OpenClient`
  reads them only from a nested `startup_params` object, so they are dropped before they reach
  the client. The connected-event rule is `expect_at_least(0)`, so nothing fails. This file
  used to claim they were "correctly parsed"; they are not exercised at all. Note the client's
  own parsing of them was separately broken (`get_string` where `get_optional_string` was
  meant, so any *subset* of the three failed to connect) and no test saw that either.
- Protocol name matches

**Runtime:** ~1-2 seconds

---

### Test 3: `test_mysql_client_transaction`

**Purpose:** Test transaction control (BEGIN, COMMIT, ROLLBACK)

**LLM Calls:** 2

1. Server startup
2. Client transaction sequence

**Flow:**

1. Start MySQL server
2. Client begins transaction
3. Client executes INSERT query
4. Client commits transaction
5. Verify connection and execution

**Expected Behavior:**

- Transaction commands execute in sequence
- LLM generates correct action sequence (begin → query → commit)
- Server receives transaction control commands

**Runtime:** ~1-2 seconds

---

## LLM Call Budget

**Total:** 6 LLM calls across 3 tests

- Well under the < 10 call budget
- Each test is independent (can run in parallel)

**Rationale:**

- Minimal LLM calls while covering key functionality
- Tests focus on client behavior, not exhaustive SQL coverage
- Simple queries reduce LLM complexity and test time

## Test Infrastructure

### Dependencies

**NetGet Binary:**

- Built with `--features mysql` to enable MySQL client
- Binary path: `target/debug/netget` or `target/release/netget`

**Test Helpers:**

- `start_netget_server()` - Spawns server instance
- `start_netget_client()` - Spawns client instance
- `{AVAILABLE_PORT}` - Random port allocation

**No External Services Required:**

- Tests use NetGet's own MySQL server
- No Docker containers needed (self-contained)

### Feature Gating

```rust
#[cfg(all(test, feature = "mysql"))]
mod mysql_client_tests { ... }
```

**Why:**

- Tests only run when `mysql` feature is enabled
- Prevents compilation errors when feature is disabled
- Follows NetGet's feature-gated test pattern

## Running Tests

### Single Protocol

```bash
./cargo-isolated.sh test --no-default-features --features mysql --test client::mysql::e2e_test
```

### With Logging

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features mysql --test client::mysql::e2e_test -- --nocapture
```

### Build Isolation

Always use `cargo-isolated.sh` to avoid conflicts with concurrent cargo processes.

## Expected Runtime

**Per Test:** 1-2 seconds
**Full Suite:** 3-6 seconds

Fast because:

- Minimal LLM calls (simple queries)
- No complex data setup
- Local server (no network latency)
- Small result sets

## Known Issues

### 1. Server Response Format

NetGet's MySQL server may respond differently than real MySQL. Tests verify connection and command execution, not exact
response format.

**Mitigation:** Tests focus on client behavior (sending queries, parsing responses) not server correctness.

### 2. LLM Variability

LLM may generate slightly different SQL syntax each run.

**Mitigation:**

- Simple queries (SELECT 1) are deterministic
- Tests verify connection, not exact query text
- Prompt engineering to guide LLM

### 3. Port Conflicts

Random port allocation may fail if ports are in use.

**Mitigation:**

- `{AVAILABLE_PORT}` finds free ports
- Tests cleanup properly
- Use `cargo-isolated.sh` for build isolation

### 4. Timeout Issues

Tests may timeout if LLM is slow or server doesn't start.

**Mitigation:**

- 500ms sleep buffers for startup
- Can increase timeouts if needed
- Tests fail fast if server/client can't start

## Future Enhancements

### 1. Real MySQL Server Tests

Connect to Docker MySQL container for full protocol compliance:

```bash
docker run -d -p 3306:3306 -e MYSQL_ROOT_PASSWORD=test mysql:8
```

**Benefits:**

- Full MySQL protocol verification
- Test against real-world server
- Catch protocol incompatibilities

**Challenges:**

- Requires Docker
- Slower (container startup)
- More complex test setup

### 2. Prepared Statements

Test prepared statement execution:

```rust
let client_config = NetGetConfig::new(format!(
    "Connect to MySQL. Execute prepared statement: SELECT * FROM users WHERE id = ?",
));
```

**Note:** Requires prepared statement support in client implementation.

### 3. Error Handling

Test invalid queries, connection failures, authentication errors:

```rust
let client_config = NetGetConfig::new(format!(
    "Connect to MySQL with invalid password. Verify error handling.",
));
```

### 4. Large Result Sets

Test queries returning thousands of rows:

```rust
let client_config = NetGetConfig::new(format!(
    "Connect to MySQL. Query 10,000 rows and analyze results.",
));
```

**Challenge:** LLM may struggle with large result sets, need pagination/streaming.

### 5. Multi-Statement Queries

Test executing multiple statements in one query:

```sql
CREATE TABLE temp (id INT); INSERT INTO temp VALUES (1);
```

**Note:** Requires server and client support for multi-statement execution.

## Debugging Tips

### View Full Output

```bash
./cargo-isolated.sh test --features mysql --test client::mysql::e2e_test -- --nocapture
```

### Check Server Logs

Server output is captured in test helpers. Print with:

```rust
println!("Server output: {:?}", server.get_output().await);
```

### Verify Protocol Registration

Ensure MySQL client is registered:

```bash
./cargo-isolated.sh run --features mysql -- --help
```

Look for MySQL in client protocol list.

### Check Connection Details

Add debug logging to client:

```rust
RUST_LOG=netget::client::mysql=trace ./cargo-isolated.sh test ...
```

## Maintenance Notes

**Dependencies:**

- Tests depend on `mysql_async` crate (via Cargo.toml)
- Feature-gated on `mysql` feature
- Require NetGet binary built with MySQL support

**Test Isolation:**

- Each test spawns independent server/client instances
- Random ports prevent conflicts
- Cleanup ensures no lingering processes

**Update Frequency:**

- Update when client implementation changes
- Add tests for new actions (prepared statements, etc.)
- Keep LLM call count < 10 total

## References

- Implementation: `src/client/mysql/CLAUDE.md`
- Test Helpers: `tests/helpers.rs`
- MySQL Protocol: https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_basics.html
