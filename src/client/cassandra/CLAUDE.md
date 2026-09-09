# Cassandra Client Implementation

## Overview

The Cassandra client implementation provides LLM-controlled access to Cassandra and ScyllaDB servers using the ScyllaDB
Rust driver. The LLM can execute CQL queries and interpret results.

**Client Library**: `scylla` v0.15+ (ScyllaDB Rust driver)
**Protocol**: CQL Binary Protocol v4
**Port**: 9042 (default Cassandra/ScyllaDB port)

## Implementation Details

### Library Choice

**ScyllaDB Rust Driver (`scylla` crate)**:

- Production-ready (v1.0 released 2025)
- Fully async with Tokio integration
- Shard-aware routing (optimized for ScyllaDB)
- Compatible with Apache Cassandra
- Built-in TLS support via Rustls
- Automatic connection pooling
- Prepared statement support
- Compression support (LZ4, Snappy)

**Why ScyllaDB driver over cdrs-tokio**:

- More actively maintained
- Better performance benchmarks
- Native Tokio integration
- Production-ready versioning (1.x vs 0.x)
- Official driver from ScyllaDB team

### Architecture

```
┌────────────────────────────────────────┐
│  CassandraClient::connect_with_llm_    │
│  - Build SessionBuilder                │
│  - Configure authentication            │
│  - Set keyspace                        │
│  - Connect to Cassandra                │
│  - Call LLM with connected event       │
└────────────────────────────────────────┘
         │
         ├─► Request-Response Model
         │   - No continuous read loop
         │   - Queries executed on-demand
         │   - LLM triggered after each result
         │
         └─► State Machine
             - Idle: Ready for queries
             - Processing: Query executing
             - Prevents concurrent LLM calls
```

### LLM Control

**Async Actions** (user-triggered):

- `execute_cql_query` - Execute CQL query
    - Parameters: query (string), consistency (optional)
    - Examples: "SELECT * FROM system.local", "INSERT INTO users VALUES (1, 'Alice')"
- `disconnect` - Close connection
- `wait_for_more` - Wait for next action

**Sync Actions** (in response to results):

- `execute_cql_query` - Execute follow-up query based on results
- `wait_for_more` - Do nothing

**Events:**

- `cassandra_connected` - Fired when connection established
    - Data includes: remote_addr
- `cassandra_result_received` - Fired when query results received
    - Data includes: rows (array), row_count (number)

### Connection Management

**Startup Parameters** (optional):

- `keyspace`: Default keyspace to use
- `username`: Username for authentication
- `password`: Password for authentication

**Session Building**:

```rust
SessionBuilder::new()
    .known_node(&remote_addr)
    .compression(Some(Compression::Lz4))
    .user(&username, &password)  // Optional
    .use_keyspace(&keyspace, false)  // Optional
    .build()
```

**Connection Lifecycle**:

1. Parse startup parameters (keyspace, auth)
2. Build SessionBuilder with configuration
3. Connect to Cassandra cluster
4. Call LLM with `cassandra_connected` event
5. Execute initial actions
6. Spawn background task for keepalive monitoring
7. Execute queries on-demand via async actions

### State Machine

**Connection States** (prevents concurrent LLM calls):

- **Idle**: Ready to execute queries
- **Processing**: Query executing, LLM call in progress
- **Accumulating**: Not used (Cassandra is request-response)

**State Transitions**:

- Idle → Processing: When query action received
- Processing → Idle: After LLM processes result
- Skip action if already Processing

### Query Execution Flow

1. LLM returns `execute_cql_query` action
2. Check connection state (skip if Processing)
3. Set state to Processing
4. Parse query string and consistency level
5. Execute query via `session.query(query, params)`
6. Convert result rows to JSON
7. Create `cassandra_result_received` event
8. Call LLM with result event
9. Set state back to Idle
10. Execute next actions from LLM

### Result Parsing

**Query Results**:

- `query_result.rows`: Optional vector of rows
- Each row converted to JSON (simplified)
- Full row parsing would require column type information

**Current Implementation** (simplified):

```rust
let rows_data: Vec<serde_json::Value> = rows.iter()
    .map(|row| json!({ "columns": format!("{:?}", row) }))
    .collect();
```

**Future Enhancement** (typed parsing):

```rust
// Parse columns by type
for row in rows {
    let id: i32 = row.columns[0].as_int().unwrap();
    let name: String = row.columns[1].as_text().unwrap();
}
```

### Structured Actions

```json
// Query action
{
  "type": "execute_cql_query",
  "query": "SELECT * FROM system.local",
  "consistency": "ONE"
}

// Result event
{
  "event_type": "cassandra_result_received",
  "data": {
    "rows": [
      {"columns": "..."},
      {"columns": "..."}
    ],
    "row_count": 2
  }
}
```

### Dual Logging

```rust
info!("Cassandra client {} connected", client_id);         // → netget.log
status_tx.send("[CLIENT] Cassandra client connected");    // → TUI
```

## Consistency Levels

Supported consistency levels (via `consistency` parameter):

- `ONE`: One replica must respond
- `TWO`: Two replicas must respond
- `THREE`: Three replicas must respond
- `QUORUM`: Majority of replicas must respond
- `ALL`: All replicas must respond
- `LOCAL_QUORUM`: Quorum in local datacenter
- `EACH_QUORUM`: Quorum in each datacenter
- `LOCAL_ONE`: One replica in local datacenter

The level is applied to the statement (`Statement::set_consistency`), not merely logged. It
used to be read, printed and dropped under a comment claiming scylla exposes no per-statement
override — it does — so a model asking for `QUORUM` was served at the session default, which
for scylla is `LOCAL_QUORUM`, and the log said `QUORUM`. An unrecognised name is now **refused**
rather than downgraded: running at a weaker guarantee than the one requested is the failure the
fix exists to prevent.

Omitting the parameter uses the session default rather than forcing `ONE`.

**Follow-up chains are bounded** by `MAX_FOLLOWUP_DEPTH` (6). `report_result` asks the model
what to do with a result set and the answer may be another query, whose result is reported in
turn; the recursion was boxed but not capped, so a model that answered every result with
another query looped for as long as the LLM budget lasted. Boxing only makes the *type* finite.
Past the bound the query still runs — refusing it would discard work the model asked for — but
its rows are not reported, and the cap is logged at WARN and on the status stream.

## Limitations

- **Simplified Result Parsing** - Row data converted to debug string, not typed parsing
- **No Prepared Statements** - Queries sent as strings (planned enhancement)
- **No Paging** - All result rows returned at once
- **No Batching** - Single queries only
- **No Streaming** - Full result sets loaded into memory
- **No Cluster Awareness in Actions** - LLM doesn't choose coordinator node
- **No Retry Policy Configuration** - Uses driver defaults
- **No Load Balancing Control** - Uses driver defaults

## Usage Examples

### Simple Query

**User**: "Connect to Cassandra and query system.local table"

**LLM Action**:

```json
{
  "type": "execute_cql_query",
  "query": "SELECT * FROM system.local"
}
```

### Query with Consistency

**User**: "Select all users with quorum consistency"

**LLM Action**:

```json
{
  "type": "execute_cql_query",
  "query": "SELECT * FROM users",
  "consistency": "QUORUM"
}
```

### Insert Data

**User**: "Insert a new user"

**LLM Action**:

```json
{
  "type": "execute_cql_query",
  "query": "INSERT INTO users (id, name, email) VALUES (1, 'Alice', 'alice@example.com')"
}
```

### Multi-Step Query

**User**: "Get user count, then select all users"

**LLM Actions** (sequence):

```json
// First action
{
  "type": "execute_cql_query",
  "query": "SELECT COUNT(*) FROM users"
}

// After receiving result, second action
{
  "type": "execute_cql_query",
  "query": "SELECT * FROM users"
}
```

## Authentication

**Plain Text Authentication** (SASL PLAIN):

```json
{
  "username": "cassandra",
  "password": "cassandra"
}
```

Passed as startup parameters when opening client.

## Compression

**Supported**:

- LZ4 (enabled by default in implementation)
- Snappy (available via driver)
- None

**Configuration**:

```rust
SessionBuilder::new()
    .compression(Some(Compression::Lz4))
```

## Known Issues

1. **Row Parsing Simplified**: Using debug format `{:?}` instead of typed parsing
2. **No Parameter Binding**: Queries use string interpolation (SQL injection risk)
3. **No Prepared Statement Caching**: Each query parsed by server
4. **Memory Usage**: Large result sets loaded entirely into memory
5. **Error Messages**: Query errors not always parsed into structured format

## Testing Strategy

See `tests/client/cassandra/CLAUDE.md` for E2E testing approach.

## Future Enhancements

- **Typed Row Parsing** - Parse column types and convert to JSON properly
- **Prepared Statements** - Cache prepared statements for reuse
- **Parameter Binding** - Use `?` placeholders and bind parameters safely
- **Paging Support** - Fetch results page by page for large queries
- **Batch Operations** - Execute multiple queries in a batch
- **Streaming Results** - Stream large result sets to LLM incrementally
- **TLS Configuration** - Allow custom TLS certificates
- **Retry Policy** - Configure retry behavior for failed queries
- **Metrics Exposure** - Expose driver metrics to LLM

## References

- [ScyllaDB Rust Driver](https://github.com/scylladb/scylla-rust-driver)
- [ScyllaDB Docs](https://rust-driver.docs.scylladb.com/)
- [CQL Reference](https://cassandra.apache.org/doc/latest/cassandra/cql/)
- [Cassandra Protocol Spec](https://github.com/apache/cassandra/blob/trunk/doc/native_protocol_v4.spec)

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` can inject an action into a running Cassandra client. The channel is
registered with `command_support::register_command_channel` **before** the
`cassandra_connected` LLM call and drained by its own task (`command_loop`), registered through
`register_client_task`.

Registering first matters more here than anywhere else in this family: this protocol *awaits*
its connect-event LLM call inline in `connect_with_llm_actions`, so under a manual `*` routing
rule client creation itself parks. The command loop is a separate task precisely so `[ send ]`
still reaches the session during that park.

Both the LLM path (`execute_actions`) and injected commands go through one `apply_action`, so
an injected `execute_cql_query` produces the same wire traffic as one the model asked for, and
the `cassandra_result_received` event fires either way. On the injected path that event is
raised **after** the outcome is replied.

**Outcome semantics** (`ClientSendOutcome`):

| Situation | Outcome |
|---|---|
| `execute_cql_query` ran | `Executed { detail }` — row count plus the truncated CQL |
| a result with no wire effect (`wait_for_more`, unhandled custom) | `Executed { detail }` naming which |
| unknown action type or bad parameters | `Rejected { error }` |
| `disconnect` | `Disconnected`, then the loop ends and the handle is dropped |
| the statement failed | `Err` — `send_to_client` returns the error |

**Never `Sent`.** The scylla driver owns the socket and its connection pool, so NetGet cannot
know how many bytes a statement put on the wire. Every loop exit calls `remove_client_handle`.

The command loop replaced the old 30-second "keepalive" polling task, and it also holds the
`Arc<Session>` — which is what keeps the session alive at all. Previously the only `Arc<Session>`
belonged to the connect-time LLM path and was dropped when `connect_with_llm_actions` returned.

Test: `tests/client/cassandra/command_channel_test.rs` (zero LLM calls; a NetGet Cassandra
server with a `*` static handler receives the injected statement).
