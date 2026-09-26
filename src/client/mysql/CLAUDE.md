# MySQL Client Implementation

## Overview

The MySQL client allows NetGet to connect to MySQL database servers and execute SQL queries under LLM control. The LLM
can execute SELECT, INSERT, UPDATE, DELETE queries, manage transactions, and analyze query results.

## Library Choice

**Primary Library:** `mysql_async` v0.34

**Rationale:**

- **Async-first design** - Built on Tokio, integrates seamlessly with NetGet's async architecture
- **Full protocol support** - Implements MySQL wire protocol with all data types
- **Connection pooling** - Efficient connection management (though we use single connections)
- **Mature crate** - Well-maintained, widely used in production
- **Type conversion** - Automatic conversion between MySQL and Rust types

**Alternative Considered:** `sqlx`

- More generic (supports multiple databases)
- Compile-time query checking with macros
- Rejected because: More complex for our use case, requires compile-time database connection for query validation

## Architecture

### Connection Model

**Type:** Single persistent connection per client instance

**Flow:**

1. Parse startup params (username, password, database)
2. Build `OptsBuilder` with connection details
3. Establish `Conn` to MySQL server
4. Wrap connection in `Arc<Mutex<Conn>>` for shared access
5. Call LLM with `mysql_connected` event
6. Execute queries asynchronously based on LLM actions

**No Read Loop:** Unlike TCP/Redis clients, MySQL client is **query-response** based:

- LLM initiates queries via actions
- Queries are executed synchronously (await response)
- Results trigger new LLM call with `mysql_result_received` event
- No background read loop needed (connection is idle between queries)

### State Machine

Uses client connection state machine (Idle → Processing):

- **Idle:** Ready to execute queries
- **Processing:** LLM call in progress, queue new queries
- No "Accumulating" state (MySQL has discrete query/response, not streaming)

State transitions:

1. LLM action → `try_start_client_llm_call()` → Processing
2. Execute query → Get result
3. Call LLM with result → `finish_client_llm_call()` → Idle

### Data Flow

```
User Instruction
    ↓
LLM generates action: execute_query("SELECT * FROM users")
    ↓
execute_llm_action() → Check state machine
    ↓
Execute query via mysql_async
    ↓
Convert Row results to JSON
    ↓
Call LLM with mysql_result_received event
    ↓
LLM analyzes results, generates next action
    ↓
Loop
```

## LLM Integration

### Event Types

**1. `mysql_connected`**

- **Trigger:** Initial connection to MySQL server
- **Data:** `remote_addr` (server address)
- **LLM Decision:** Execute initial query, set up schema, begin transaction

**2. `mysql_result_received`**

- **Trigger:** Query result received from server
- **Data:**
    - `query`: the SQL this result belongs to
    - `result`: array of row objects keyed by column name (the text protocol carries every
      value as a string, so an `INT` id arrives as `"1"`)
    - `row_count`: rows returned
    - `affected_rows`: what the server's OK packet says (1 for a one-row `INSERT`, 0 for a
      `SELECT`) — read with `Conn::affected_rows()` under the same guard as the query, because
      the next query overwrites it
    - `last_insert_id`: the `AUTO_INCREMENT` id the statement generated, only when it
      generated one

  `tests/client/mysql/real_server_test.rs` matches a real `mysqld`'s INSERT result on
  `affected_rows` 1 and `last_insert_id` 1, which a row count (0 for an `INSERT`) cannot
  satisfy.
- **LLM Decision:** Analyze results, execute follow-up queries, commit/rollback transaction

**The answer to this event is executed**, and that was not always true. The loop used to call
`protocol.execute_action(..)` — which is pure, returning a `Custom { name: "mysql_query" }`
that nothing put on the connection — and then `trace!` everything but `Disconnect`. So a model
answering `mysql_result_received` with `execute_query` issued no query and sent no bytes, on
the LLM path and on the dashboard's `[ send ]` path alike.

Closing the loop makes it self-referential (a query raises a result event, whose answer may be
another query), so `execute_llm_action` returns an explicitly boxed `+ Send` future — an
`async fn` that awaits itself is infinitely sized (E0391), and two mutually-recursive `async
fn`s defeat `Send` inference — and the chain is capped at `MAX_FOLLOWUP_DEPTH` (4). Past the
cap the actions are dropped with a WARN rather than looping forever.
`tests/client/mysql/e2e_test.rs::a_follow_up_query_from_the_model_actually_reaches_the_server`
asserts it from the server's side, which is the only place the effect is real.

### Actions

**Async Actions (User-Triggered):**

1. `execute_query` - Execute any SQL query
    - Parameters: `query` (SQL string)
    - Example: `SELECT * FROM users WHERE id = 1`

2. `begin_transaction` - Start a transaction
    - Executes: `BEGIN`

3. `commit_transaction` - Commit current transaction
    - Executes: `COMMIT`

4. `rollback_transaction` - Rollback current transaction
    - Executes: `ROLLBACK`

5. `disconnect` - Close connection

**Sync Actions (Response-Triggered):**

1. `execute_query` - Execute query based on previous result
2. `wait_for_more` - Wait without executing new queries

### Structured Data Design

**CRITICAL:** No base64-encoded binary data. All queries are SQL strings (text).

**Query Examples:**

```json
{
  "type": "execute_query",
  "query": "SELECT id, name, email FROM users WHERE active = 1"
}
```

**Result Format:**

```json
{
  "query": "SELECT id, name FROM users",
  "result": [
    {"id": "1", "name": "Alice"},
    {"id": "2", "name": "Bob"}
  ],
  "row_count": 2,
  "affected_rows": 0
}
```

### Type Conversion

`Conn::query` is the **text** protocol, so a real server sends every non-NULL value as a
string and `rows_to_json` passes it through: `INT` `1` arrives as `"1"`, `DATETIME` as
`"2026-09-26 06:00:00"`, `BLOB` as a UTF-8-lossy string, `NULL` as `null`. The numeric and
date arms in `rows_to_json` only fire for binary-protocol values, which this client never
requests.

## Startup Parameters

**Optional parameters** when opening client:

1. `username` - MySQL username (default: "root")
2. `password` - MySQL password (default: "")
3. `database` - Initial database to connect to (default: none)

**Example:**

```json
{
  "username": "myuser",
  "password": "mypass",
  "database": "mydb"
}
```

## Query Execution

### Simple Queries

LLM generates SQL, client executes via `conn.query()`:

```rust
let result: Vec<Row> = conn_guard.query(query_str).await?;
```

Results are converted to JSON arrays and sent to LLM.

### Transactions

LLM controls transaction boundaries:

1. `begin_transaction` → Execute `BEGIN`
2. Multiple `execute_query` actions within transaction
3. `commit_transaction` → Execute `COMMIT`
   OR
   `rollback_transaction` → Execute `ROLLBACK`

### Error Handling

Query errors:

- Set client status to `Error(message)` and update the UI
- Do NOT disconnect
- Raise **no** event: the model is not told the query failed, so it cannot correct it

## Limitations

### 1. No Prepared Statements (Yet)

Current implementation uses simple text queries. Prepared statements could be added:

```rust
conn.exec("SELECT * FROM users WHERE id = ?", (user_id,)).await?;
```

**Why not implemented:** LLM generates SQL strings naturally, prepared statements add complexity.

### 2. Single Connection

No connection pooling. Each client instance has one connection. For high concurrency, open multiple client instances.

### 3. No Streaming Results

Large result sets are loaded entirely into memory. For huge queries (millions of rows), consider:

- LIMIT clauses
- Pagination
- Streaming with `query_iter()`

### 4. Limited Type Support

Complex types (JSON, GEOMETRY, ENUM) are converted to strings. For precise handling, enhance type conversion.

### 5. No Binary Protocol Features

Uses text protocol. Binary protocol (for prepared statements) not utilized.

## Security Considerations

### SQL Injection

**CRITICAL:** LLM generates raw SQL strings. Potential for injection if user input flows to LLM without sanitization.

**Mitigations:**

- LLM prompt engineering: "Never execute DROP, DELETE FROM without WHERE clause"
- Read-only user accounts
- Database permissions
- Future: Prepared statements with parameter binding

### Credential Management

Credentials passed in startup params. For production:

- Use secrets management
- Environment variables
- Credential vaulting

## Example Prompts

### 1. Simple Query

```
Connect to MySQL at localhost:3306 as root with password 'secret' and database 'testdb'.
Query the users table and show all active users.
```

LLM Flow:

1. Connected → `execute_query("SELECT * FROM users WHERE active = 1")`
2. Result received → Analyze rows → `disconnect`

### 2. Transaction

```
Connect to MySQL at localhost:3306 and transfer $100 from account 1 to account 2.
Use a transaction to ensure atomicity.
```

LLM Flow:

1. Connected → `begin_transaction`
2. `execute_query("UPDATE accounts SET balance = balance - 100 WHERE id = 1")`
3. Result → `execute_query("UPDATE accounts SET balance = balance + 100 WHERE id = 2")`
4. Result → `commit_transaction`
5. `disconnect`

### 3. Schema Analysis

```
Connect to MySQL at localhost:3306 and describe the structure of the 'products' table.
```

LLM Flow:

1. Connected → `execute_query("DESCRIBE products")`
2. Result → Analyze schema → `disconnect`

## Future Enhancements

### 1. Prepared Statements

Add `execute_prepared` action:

```json
{
  "type": "execute_prepared",
  "query": "SELECT * FROM users WHERE id = ?",
  "params": [123]
}
```

### 2. Streaming Results

For large result sets:

```rust
let mut result = conn.query_iter(query_str).await?;
while let Some(row) = result.next().await? {
    // Send row to LLM incrementally
}
```

### 3. Multi-Statement Queries

Execute multiple statements in one call:

```sql
CREATE TABLE temp (id INT); INSERT INTO temp VALUES (1);
```

### 4. Connection Pooling

Use `mysql_async::Pool` for better resource management:

```rust
let pool = Pool::new(opts);
let conn = pool.get_conn().await?;
```

### 5. LOAD DATA INFILE

Bulk data loading for LLM-generated datasets.

## Testing Strategy

See `tests/client/mysql/CLAUDE.md` for E2E test details.

The evidence is `tests/client/mysql/real_server_test.rs`: a real `mysqld` initialised per
test, driven through CREATE / INSERT / SELECT / an INSERT built from the SELECT's rows, and
read back with the `mysql` CLI. No Docker.

## Maintenance Notes

**Dependencies:**

- `mysql_async = "0.34"` - Main MySQL client library
- Feature-gated in `Cargo.toml` under `[features]`

**Code Organization:**

- `mod.rs` - Connection logic, query execution, LLM integration
- `actions.rs` - `Client` trait implementation, action definitions
- `CLAUDE.md` - This document

**Common Issues:**

1. **Connection refused** - Ensure MySQL is running, check host/port
2. **Access denied** - Verify username/password
3. **Unknown database** - Check database exists or omit database param
4. **Query errors** - LLM generates invalid SQL, improves with better prompting

## References

- [mysql_async crate](https://docs.rs/mysql_async/)
- [MySQL Protocol](https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_basics.html)
- [SQL Injection Prevention](https://cheatsheetseries.owasp.org/cheatsheets/SQL_Injection_Prevention_Cheat_Sheet.html)

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` can inject an action into a running MySQL client. The channel is
registered with `command_support::register_command_channel` **before** the `mysql_connected`
LLM call and drained by its own task (`command_loop`), registered through
`register_client_task`. Registering first is what makes `[ send ]` usable while a manual `*`
routing rule has the connect event parked waiting for a human.

Both the LLM path and injected commands go through one `apply_action`, so an injected
`execute_query` produces byte-for-byte the same wire traffic as one the model asked for, and
the `mysql_result_received` event fires either way. On the injected path that event is raised
**after** the outcome is replied, so `[ send ]` is not held for an LLM round-trip.

**Outcome semantics** (`ClientSendOutcome`):

| Situation | Outcome |
|---|---|
| `execute_query` / `begin`/`commit`/`rollback_transaction` ran | `Executed { detail }` — row count plus the truncated SQL |
| `wait_for_more`, or a result with no wire effect | `Executed { detail }` naming which |
| unknown action type or bad parameters | `Rejected { error }` |
| `disconnect` | `Disconnected`, then the loop ends and the handle is dropped |
| the query failed on the server | `Err` — `send_to_client` returns the error |

**Never `Sent`.** `mysql_async` owns the socket, so NetGet cannot know how many bytes a query
put on the wire; reporting a byte count would be a guess. The command loop's exit — an
injected `disconnect`, or `remove_client`/`disconnect_client` dropping the sender — calls
`remove_client_handle`, so the dashboard stops offering `[ send ]` into a dead client.

The command loop also holds the `Arc<Mutex<Conn>>`, which is what keeps the connection alive
for a client created with no instruction; before it existed the only `Conn` was dropped when
`connect_with_llm_actions` returned.

### Connection options are pinned on purpose

`OptsBuilder` sets `max_allowed_packet`, `wait_timeout` and `prefer_socket(false)`. Left
unset, `Conn::new` runs `SELECT @@max_allowed_packet, @@wait_timeout, @@socket` and feeds each
answer to `from_value`, which **panics** rather than erroring when the value is not the type it
expects. The server a NetGet client talks to is often one whose replies a model composed, so
that panic is reachable from an ordinary LLM answer — connecting NetGet's MySQL client to
NetGet's own MySQL server used to abort the task. Supplying the values means the settings query
is never issued.

Test: `tests/client/mysql/command_channel_test.rs` (zero LLM calls; a NetGet MySQL server with
a `*` static handler receives the injected query).

## Maturity: Beta

Rated against the four-condition client bar in the root `CLAUDE.md`, on the evidence in
`tests/client/mysql/real_server_test.rs` (see `tests/client/mysql/CLAUDE.md`):

1. **Real third-party server** — Oracle's `mysqld` (C++), initialised per test with
   `--initialize-insecure` and read back with the `mysql` CLI (libmysqlclient). NetGet's side
   is `mysql_async`, which shares no code with either. (NetGet's own MySQL *server* is
   `opensrv-mysql`; it is not involved.)
2. **Fails rather than skips** — a missing `mysqld` or `mysql` is a test failure naming the brew
   formula and the Ubuntu package (`tests/helpers/real_server.rs`); nothing is `#[ignore]`d.
   CI's `registry-audit` installs the server and runs the suite in its evidence loop.
3. **A real session** — the handshake and `caching_sha2_password` authentication of a real
   MySQL 9 server, the `database` startup parameter, then `CREATE TABLE`, `INSERT` and
   `SELECT` with the OK-packet counters and the rows handed to the model.
4. **Acts on the model's answer, asserted on the wire** — the `mysql` CLI reads back the table
   the model created, the row it inserted, and a second row it built from the `SELECT` result
   it was shown. Verified by mutation: dropping the actions the model returns for a result
   makes the test fail.

Not covered by that evidence: TLS, a password (the test `root` has none), error reporting to
the model, and prepared statements.
