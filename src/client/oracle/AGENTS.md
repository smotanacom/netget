# Oracle Client Implementation

**Status:** Planned (Not Yet Implemented)
**Complexity:** 🟡 **Medium**
**Library:** `rust-oracle` v0.6.2
**Protocol:** Oracle TNS/TTC (handled by ODPI-C)

---

## Overview

This document describes the planned implementation of an Oracle database client that connects to Oracle databases (including NetGet Oracle server or real Oracle instances) and allows an LLM to control SQL query execution.

**Key Advantage:** Unlike the server implementation, we have a mature Rust library (`rust-oracle`) for Oracle client connectivity.

---

## Library Choice: rust-oracle

### Why rust-oracle?

**Crate:** `rust-oracle` v0.6.2
**GitHub:** https://github.com/kubo/rust-oracle
**Docs:** https://docs.rs/oracle/latest/oracle/

**Advantages:**
- ✅ **Mature** - Actively maintained, production-ready
- ✅ **ODPI-C Based** - Uses Oracle's official C driver (ODPI-C)
- ✅ **Full Oracle Support** - Oracle 11.2+ compatible
- ✅ **All SQL Operations** - Query, execute, prepared statements, transactions
- ✅ **Connection Pooling** - `r2d2-oracle` available
- ✅ **Well Documented** - 88% documentation coverage
- ✅ **Stable** - Minimum Rust 1.60.0, supports 6+ minor versions

**Requirements:**
- Oracle Instant Client or Oracle Database installed
- Client libraries (libclntsh.so on Linux, oci.dll on Windows)
- Environment variables: `LD_LIBRARY_PATH` (Linux) or `PATH` (Windows)

### Alternatives Considered

| Crate | Status | Decision |
|-------|--------|----------|
| `rust-oracle` | ✅ Mature, v0.6.2 | **SELECTED** |
| `oci_rs` | 🟡 OCI wrapper, less mature | Not selected (lower-level) |
| `sibyl` | 🟡 Alternative driver | Not selected (less popular) |

---

## Architecture

### Client Design Pattern

Following the **Redis client pattern** from `src/client/redis/`:

1. **Connection via library** (`oracle::Connection::connect()`)
2. **LLM integration loop** (connect → event → LLM → actions → event → ...)
3. **Action execution** (execute SQL, commit, rollback)
4. **Result events** (query results sent back to LLM)

### Key Difference from Server

**Server:** Must parse TNS/TTC manually (no library)
**Client:** Library handles all protocol details (easy!)

```
┌─────────────────────────────────┐
│  LLM (Controls Query Execution) │
└────────────┬────────────────────┘
             │ Actions
┌────────────▼────────────────────┐
│  Oracle Client Wrapper          │
│  - Connection management        │
│  - Execute SQL via LLM actions  │
│  - Parse results → Events       │
└────────────┬────────────────────┘
             │ oracle::Connection
┌────────────▼────────────────────┐
│  rust-oracle (ODPI-C)           │
│  - TNS/TTC protocol             │
│  - Oracle wire format           │
└────────────┬────────────────────┘
             │ TCP
┌────────────▼────────────────────┐
│  Oracle Database Server         │
│  (NetGet or Real Oracle)        │
└─────────────────────────────────┘
```

---

## Key Hardship: Blocking I/O

### The Challenge

**rust-oracle is synchronous (blocking), not async.**

All operations block the current thread:
```rust
// These block:
let conn = Connection::connect(user, pass, connect_string)?;
let rows = conn.query("SELECT * FROM employees", &[])?;
let result = conn.execute("INSERT INTO ...", &[])?;
```

**NetGet is async** (uses Tokio runtime).

### Solution: tokio::task::spawn_blocking

Wrap all blocking oracle operations in `spawn_blocking`:

```rust
// Async wrapper for blocking oracle operations
pub async fn connect_with_llm_actions(...) -> Result<SocketAddr> {
    // Connect (blocking) - must use spawn_blocking
    let conn = tokio::task::spawn_blocking({
        let connection_string = connection_string.clone();
        let username = username.clone();
        let password = password.clone();

        move || {
            Connection::connect(&username, &password, &connection_string)
        }
    }).await??; // Note: double ? (JoinError + OracleError)

    // ... rest of async code ...
}

// Execute query (blocking) - must use spawn_blocking
let result = tokio::task::spawn_blocking({
    let sql = sql.to_string();
    // SAFETY: conn is valid for this block
    let conn_ptr = &conn as *const Connection as usize;

    move || {
        let conn = unsafe { &*(conn_ptr as *const Connection) };
        conn.query(&sql, &[])
    }
}).await??;
```

**Why This Works:**
- `spawn_blocking` runs blocking code on a separate thread pool
- Doesn't block the Tokio async runtime
- Small performance overhead, but acceptable for database I/O

**Alternative Considered:**
- Wait for async Oracle driver (none exists as of 2025)
- Use async wrapper around oracle crate (would still need spawn_blocking)

---

## LLM Integration

### Connection Flow

```
1. User: "Connect to Oracle at oracle.example.com:1521"
2. Client connects via rust-oracle
3. LLM receives oracle_connected event:
   {
     "event_type": "oracle_connected",
     "event_data": {
       "remote_addr": "oracle.example.com:1521/ORCL",
       "username": "system",
       "service_name": "ORCL"
     }
   }
4. LLM decides next action (query tables, execute SQL, etc.)
```

### Query Execution Flow

```
1. LLM returns action:
   {
     "type": "execute_oracle_query",
     "sql": "SELECT employee_id, first_name FROM employees WHERE department_id = 50"
   }
2. Client executes via spawn_blocking:
   let rows = conn.query(sql, &[])?;
3. Client parses results (columns + rows)
4. Client calls LLM with oracle_query_result event:
   {
     "event_type": "oracle_query_result",
     "event_data": {
       "sql": "SELECT ...",
       "row_count": 3,
       "rows": [
         ["100", "Steven"],
         ["101", "Neena"],
         ["102", "Lex"]
       ]
     }
   }
5. LLM decides next action (more queries, commit, disconnect, etc.)
```

### LLM Control Points

The LLM controls:
- ✅ **SQL Execution** (SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, etc.)
- ✅ **Transaction Management** (COMMIT, ROLLBACK)
- ✅ **Query Analysis** (interpret results, decide next steps)
- ✅ **Error Handling** (react to Oracle errors)
- ✅ **Connection Lifecycle** (when to disconnect)

**No Hardcoding:** LLM generates SQL dynamically based on user intent and previous results.

---

## Actions (Client)

### execute_oracle_query

**Purpose:** Execute SQL query or DML statement

**Parameters:**
- `sql`: SQL string (SELECT, INSERT, UPDATE, DELETE, CREATE, etc.)

**Example (SELECT):**
```json
{
  "type": "execute_oracle_query",
  "sql": "SELECT employee_id, first_name, salary FROM employees WHERE department_id = 50 ORDER BY salary DESC"
}
```

**Example (INSERT):**
```json
{
  "type": "execute_oracle_query",
  "sql": "INSERT INTO employees (employee_id, first_name, last_name, hire_date) VALUES (200, 'John', 'Doe', DATE '2025-11-20')"
}
```

**Example (DDL):**
```json
{
  "type": "execute_oracle_query",
  "sql": "CREATE TABLE test_table (id NUMBER PRIMARY KEY, name VARCHAR2(100))"
}
```

**Implementation:**
```rust
async fn execute_query_action(sql: &str, conn: &Connection) -> Result<QueryResult> {
    tokio::task::spawn_blocking({
        let sql = sql.to_string();
        let conn_ptr = conn as *const Connection as usize;

        move || {
            let conn = unsafe { &*(conn_ptr as *const Connection) };

            // Determine if SELECT or DML/DDL
            let sql_upper = sql.trim().to_uppercase();
            if sql_upper.starts_with("SELECT") {
                // Query - return rows
                let rows = conn.query(&sql, &[])?;
                let mut result_rows = Vec::new();

                for row_result in rows {
                    let row = row_result?;
                    let mut row_values = Vec::new();

                    // Extract all columns as strings (simplified)
                    for i in 0..row.column_info().len() {
                        let value: Option<String> = row.get(i)?;
                        row_values.push(value.unwrap_or_else(|| "NULL".to_string()));
                    }

                    result_rows.push(row_values);
                }

                Ok(QueryResult::Rows(result_rows))
            } else {
                // DML/DDL - return affected rows
                let result = conn.execute(&sql, &[])?;
                let rows_affected = result.row_count()?;
                Ok(QueryResult::Affected(rows_affected))
            }
        }
    }).await?
}
```

### oracle_commit

**Purpose:** Commit current transaction

**Parameters:** None

**Example:**
```json
{
  "type": "oracle_commit"
}
```

**Implementation:**
```rust
tokio::task::spawn_blocking({
    let conn_ptr = conn as *const Connection as usize;
    move || {
        let conn = unsafe { &*(conn_ptr as *const Connection) };
        conn.commit()
    }
}).await??;
```

### oracle_rollback

**Purpose:** Rollback current transaction

**Parameters:** None

**Example:**
```json
{
  "type": "oracle_rollback"
}
```

**Implementation:**
```rust
tokio::task::spawn_blocking({
    let conn_ptr = conn as *const Connection as usize;
    move || {
        let conn = unsafe { &*(conn_ptr as *const Connection) };
        conn.rollback()
    }
}).await??;
```

### disconnect

**Purpose:** Close Oracle connection

**Parameters:** None

**Example:**
```json
{
  "type": "disconnect"
}
```

**Implementation:**
```rust
// Connection is dropped when client is closed
// No explicit disconnect needed (RAII)
```

---

## Events (Client)

### oracle_connected

**Triggered When:** Successfully connected to Oracle database

**Event Data:**
- `remote_addr`: Connection string (e.g., "oracle.example.com:1521/ORCL")
- `username`: Oracle username used for connection
- `service_name`: Oracle service name

**Available Actions:**
- `execute_oracle_query` - Execute SQL
- `oracle_commit` - Commit transaction
- `oracle_rollback` - Rollback transaction
- `disconnect` - Close connection

**Example:**
```json
{
  "event_type": "oracle_connected",
  "event_data": {
    "remote_addr": "oracle.example.com:1521/ORCL",
    "username": "system",
    "service_name": "ORCL"
  }
}
```

### oracle_query_result

**Triggered When:** SQL query executed, results received

**Event Data:**
- `sql`: SQL query that was executed
- `rows`: Result rows (for SELECT queries, array of arrays)
- `row_count`: Number of rows returned (for SELECT)
- `rows_affected`: Number of rows affected (for INSERT/UPDATE/DELETE)

**Available Actions:**
- `execute_oracle_query` - Execute another query
- `oracle_commit` - Commit transaction
- `oracle_rollback` - Rollback transaction
- `disconnect` - Close connection

**Example (SELECT result):**
```json
{
  "event_type": "oracle_query_result",
  "event_data": {
    "sql": "SELECT employee_id, first_name FROM employees WHERE department_id = 50",
    "row_count": 3,
    "rows": [
      ["100", "Steven"],
      ["101", "Neena"],
      ["102", "Lex"]
    ]
  }
}
```

**Example (DML result):**
```json
{
  "event_type": "oracle_query_result",
  "event_data": {
    "sql": "INSERT INTO employees (...) VALUES (...)",
    "rows_affected": 1
  }
}
```

---

## Startup Parameters

### Configuration

**Parameters:**
- `username`: Oracle username (default: "system")
- `password`: Oracle password (default: "oracle")
- `service_name`: Oracle service name or SID (default: "ORCL")

**Usage:**
```bash
# NetGet CLI
open_client oracle oracle.example.com:1521 \
  --param username=hr \
  --param password=hr123 \
  --param service_name=HRDB \
  "List all tables in the HR schema"
```

**Connection String Format:**
```
{host}:{port}/{service_name}

Examples:
- localhost:1521/XE
- oracle.example.com:1521/ORCL
- 192.168.1.100:1522/PRODDB
```

### Environment Setup

**Oracle Instant Client Required:**

**Linux:**
```bash
# Download Oracle Instant Client
# Extract to /opt/oracle/instantclient_21_1/

# Set library path
export LD_LIBRARY_PATH=/opt/oracle/instantclient_21_1:$LD_LIBRARY_PATH

# Verify
sqlplus -V
```

**macOS:**
```bash
# Download Oracle Instant Client
# Extract to /usr/local/lib/

# Set library path
export DYLD_LIBRARY_PATH=/usr/local/lib:$DYLD_LIBRARY_PATH
```

**Windows:**
```cmd
REM Download Oracle Instant Client
REM Extract to C:\oracle\instantclient_21_1\

REM Add to PATH
set PATH=C:\oracle\instantclient_21_1;%PATH%
```

**Docker (for testing):**
```dockerfile
FROM rust:latest

# Install Oracle Instant Client
RUN apt-get update && apt-get install -y wget unzip libaio1

RUN wget https://download.oracle.com/otn_software/linux/instantclient/219000/instantclient-basic-linux.x64-21.9.0.0.0dbru.zip
RUN unzip instantclient-basic-linux.x64-21.9.0.0.0dbru.zip -d /opt/oracle
RUN sh -c "echo /opt/oracle/instantclient_21_9 > /etc/ld.so.conf.d/oracle-instantclient.conf"
RUN ldconfig

ENV LD_LIBRARY_PATH=/opt/oracle/instantclient_21_9:$LD_LIBRARY_PATH
```

---

## File Structure

```
src/client/oracle/
├── mod.rs              # Client connection logic, query execution
├── actions.rs          # Client trait implementation, events, actions
└── CLAUDE.md           # This file
```

---

## Testing Strategy

### Approach

**Option 1:** Connect to NetGet Oracle server (mock LLM responses)
**Option 2:** Connect to real Oracle database (Oracle XE in Docker)

**Recommended:** Use NetGet Oracle server for E2E tests (faster, no external dependencies)

### Test Scenarios (< 10 LLM calls total)

1. **Client Connect** (1 call) - Connect to Oracle server
2. **Execute SELECT** (1 call) - Query employees table
3. **Execute INSERT** (1 call) - Insert new employee
4. **Execute UPDATE** (1 call) - Update employee salary
5. **Transaction Commit** (1 call) - Commit changes
6. **Transaction Rollback** (1 call) - Rollback changes
7. **Disconnect** (0 calls) - Close connection

**Total:** ~6 LLM calls (well under 10 budget)

### E2E Test Example

```rust
#[cfg(all(test, feature = "oracle"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_oracle_client_query() -> Result<()> {
        // Start NetGet Oracle server with mock
        let server_config = NetGetConfig::new("Start Oracle server on port {AVAILABLE_PORT}")
            .with_mock(|mock| {
                mock
                    .on_event("oracle_query")
                    .and_event_data_contains("query", "SELECT")
                    .respond_with_actions(vec![
                        json!({
                            "type": "oracle_query_response",
                            "columns": [
                                {"name": "EMPLOYEE_ID", "type": "NUMBER"},
                                {"name": "FIRST_NAME", "type": "VARCHAR2"}
                            ],
                            "rows": [
                                [100, "Steven"],
                                [101, "Neena"]
                            ]
                        })
                    ])
                    .expect_calls(1)
                    .and()
            });

        let mut server = NetGetServer::start(server_config).await?;
        let server_port = server.get_server_port("oracle").await?;

        // Start Oracle client
        let client_config = NetGetConfig::new(&format!(
            "Connect to Oracle at localhost:{} and list employees",
            server_port
        ))
        .with_startup_param("username", "hr")
        .with_startup_param("password", "hr")
        .with_startup_param("service_name", "XE")
        .with_mock(|mock| {
            mock
                .on_event("oracle_connected")
                .respond_with_actions(vec![
                    json!({
                        "type": "execute_oracle_query",
                        "sql": "SELECT employee_id, first_name FROM employees"
                    })
                ])
                .expect_calls(1)
                .and()
                .on_event("oracle_query_result")
                .and_event_data_contains("row_count", 2)
                .respond_with_actions(vec![
                    json!({"type": "disconnect"})
                ])
                .expect_calls(1)
                .and()
        });

        let mut client = NetGetClient::start(client_config).await?;

        // Wait for completion
        client.wait_for_completion(Duration::from_secs(10)).await?;

        // Verify mocks
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        Ok(())
    }
}
```

---

## Known Limitations

### Oracle Instant Client Required

**Limitation:** rust-oracle requires Oracle Instant Client installed on the system.

**Why:** ODPI-C (underlying library) needs Oracle client libraries.

**Impact:**
- ❌ Cannot use in environments without Oracle client
- ❌ Adds ~50-200 MB to deployment size
- ❌ Licensing considerations (Oracle Instant Client license)

**Mitigation:**
- Document installation clearly
- Provide Docker images with Instant Client pre-installed
- Consider this a testing-only feature (not for production NetGet deployments)

### Blocking I/O Performance

**Limitation:** All Oracle operations block threads (not async)

**Impact:**
- ⚠️ Lower concurrency than native async (but spawn_blocking mitigates this)
- ⚠️ Thread pool overhead for each operation
- ⚠️ Slightly higher latency (~1-2ms per operation)

**Mitigation:**
- Acceptable for database I/O (network latency dominates)
- Tokio's blocking thread pool is efficient
- Not a practical issue for NetGet's use case

### Type Conversion Simplification

**Limitation:** All Oracle values converted to strings for LLM

**Why:** Simplifies JSON serialization, LLM doesn't need binary types

**Impact:**
- ⚠️ Loss of type information (NUMBER becomes "123.45" string)
- ⚠️ No distinction between VARCHAR2 and CHAR
- ⚠️ Dates/timestamps as strings (not ISO 8601 enforced)

**Mitigation:**
- Sufficient for LLM understanding
- LLM can infer types from context
- Post-MVP: Add type metadata to events

### No Connection Pooling (MVP)

**Limitation:** Each client opens one connection (no pool)

**Why:** MVP simplicity, connection pooling adds complexity

**Impact:**
- ⚠️ Cannot reuse connections across multiple client instances
- ⚠️ Higher connection overhead for multiple clients

**Future Enhancement:** Use `r2d2-oracle` for connection pooling

---

## Error Handling

### Oracle Errors

rust-oracle maps Oracle errors to `oracle::Error` enum:

```rust
match conn.execute(sql, &[]) {
    Ok(result) => { /* success */ }
    Err(oracle::Error::OciError(dberr)) => {
        // Oracle database error (ORA-XXXXX)
        let error_code = dberr.code(); // e.g., 942
        let error_message = dberr.message(); // e.g., "table or view does not exist"

        // Send to LLM as event
        let event = Event::new(
            &ORACLE_CLIENT_ERROR_EVENT,
            json!({
                "error_code": error_code,
                "message": error_message,
                "sql": sql,
            })
        );
    }
    Err(e) => {
        // Other errors (connection lost, etc.)
        error!("Oracle client error: {}", e);
    }
}
```

### Common Oracle Errors

| Error Code | Message | Cause |
|------------|---------|-------|
| ORA-00942 | table or view does not exist | Invalid table name |
| ORA-00001 | unique constraint violated | Duplicate key |
| ORA-01400 | cannot insert NULL | NOT NULL violation |
| ORA-02291 | integrity constraint violated | Foreign key violation |
| ORA-01722 | invalid number | Type mismatch |
| ORA-12154 | TNS:could not resolve connect identifier | Invalid connection string |
| ORA-01017 | invalid username/password | Authentication failure |

---

## Implementation Complexity

### Lines of Code Estimate

| Component | LOC | Notes |
|-----------|-----|-------|
| mod.rs | 250-350 | Connection, query execution, spawn_blocking wrappers |
| actions.rs | 150-200 | Event/action definitions, Client trait impl |
| **TOTAL** | **400-550** | Client implementation |

**Comparison:**
- Redis client: ~400 LOC (similar complexity)
- TCP client: ~300 LOC (simpler, just send/receive bytes)
- HTTP client: ~450 LOC (reqwest wrapper with headers/body)
- **Oracle client: ~450 LOC** (database client with transactions)

### Development Timeline

**Estimated:** 2-3 days for MVP

1. **Day 1:** Connection + basic query execution (mod.rs)
   - Connection via rust-oracle
   - spawn_blocking wrappers
   - Execute SELECT/INSERT/UPDATE/DELETE
   - Parse results

2. **Day 2:** LLM integration + actions (actions.rs)
   - Event definitions (connected, query_result)
   - Action definitions (execute_query, commit, rollback)
   - Client trait implementation
   - Action execution flow

3. **Day 3:** Testing + documentation
   - E2E tests with NetGet Oracle server
   - Mock LLM responses
   - Error handling tests
   - Complete CLAUDE.md

---

## Risks & Mitigation

### Risk 1: Oracle Instant Client Installation

**Risk:** Users may struggle to install Oracle Instant Client

**Probability:** High (unfamiliar to many developers)
**Impact:** High (client won't work without it)

**Mitigation:**
- Provide clear installation documentation
- Offer Docker images with Instant Client pre-installed
- Add "Getting Started" guide with screenshots
- Consider this an advanced feature (not for all users)

### Risk 2: Blocking I/O Performance

**Risk:** spawn_blocking may have performance issues with many concurrent clients

**Probability:** Low (NetGet is for testing, not production scale)
**Impact:** Medium (slower than native async)

**Mitigation:**
- Document performance characteristics
- Acceptable for testing use case
- Tokio's blocking thread pool handles this well

### Risk 3: Type Conversion Errors

**Risk:** Converting all Oracle types to strings may lose information

**Probability:** Medium
**Impact:** Low (LLM can infer types)

**Mitigation:**
- Sufficient for MVP (LLM-controlled use case)
- Post-MVP: Add type metadata to events
- Document conversion behavior

---

## Future Enhancements (Post-MVP)

### Priority 1: Connection Pooling

Use `r2d2-oracle` for connection pooling:

```rust
use r2d2_oracle::OracleConnectionManager;

let manager = OracleConnectionManager::new(username, password, connect_string);
let pool = r2d2::Pool::new(manager)?;

// Reuse connections
let conn = pool.get()?;
```

### Priority 2: Prepared Statements

Support bind variables for efficiency:

```rust
let mut stmt = conn.prepare("SELECT * FROM employees WHERE department_id = :1")?;
let rows = stmt.query(&[&50])?;
```

**LLM Action:**
```json
{
  "type": "execute_oracle_query",
  "sql": "SELECT * FROM employees WHERE department_id = :dept_id",
  "bind_params": {
    "dept_id": 50
  }
}
```

### Priority 3: Type Metadata

Include Oracle type information in events:

```json
{
  "event_type": "oracle_query_result",
  "event_data": {
    "columns": [
      {"name": "EMPLOYEE_ID", "oracle_type": "NUMBER", "precision": 6, "scale": 0},
      {"name": "FIRST_NAME", "oracle_type": "VARCHAR2", "max_length": 20}
    ],
    "rows": [...]
  }
}
```

### Priority 4: CLOB/BLOB Support

Handle large objects:

```rust
let clob: oracle::Clob = row.get(0)?;
let clob_data = clob.read_to_string()?; // Read all data

// Send to LLM (truncated if too large)
let preview = if clob_data.len() > 1000 {
    format!("{}... (truncated, {} bytes total)", &clob_data[..1000], clob_data.len())
} else {
    clob_data
};
```

### Priority 5: PL/SQL Support

Execute stored procedures via LLM:

```json
{
  "type": "execute_oracle_procedure",
  "procedure": "pkg_employee.update_salary",
  "params": {
    "p_employee_id": 100,
    "p_new_salary": 25000
  }
}
```

---

## Conclusion

The Oracle client implementation is **significantly easier** than the server due to the availability of `rust-oracle` crate.

**Key Advantages:**
- ✅ Mature library handles all protocol complexity
- ✅ Full Oracle compatibility (11.2+)
- ✅ Simple LLM integration (query → result event loop)

**Key Hardships:**
- ⚠️ Blocking I/O (mitigated with spawn_blocking)
- ⚠️ Oracle Instant Client dependency (installation overhead)
- ⚠️ Type conversion simplification (acceptable for LLM use case)

**Overall:** Medium complexity (2-3 days for MVP), well-suited for NetGet's testing-focused use case.

---

**Last Updated:** 2025-11-20
**Status:** Planning Complete, Implementation Not Started
**See Also:**
- `docs/archive/ORACLE_PROTOCOL_PLAN.md` (comprehensive plan)
- `/home/user/netget/src/server/oracle/CLAUDE.md` (server implementation)
