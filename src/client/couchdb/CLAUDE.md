# CouchDB Client Implementation

## Overview

CouchDB client that connects to CouchDB servers and allows the LLM to control database operations, document CRUD, bulk operations, and view queries. The client uses the `couch_rs` library and provides full LLM control over all CouchDB operations.

**Default Port**: 5984
**Protocol**: HTTP/1.1 with JSON payloads
**Library**: `couch_rs` v0.10
**Stack**: `ETH>IP>TCP>HTTP>COUCHDB`

## Library Choices

**couch_rs** (v0.10):
- Modern Rust CouchDB client library
- Async/await with tokio
- Based on reqwest (HTTP client)
- Supports CouchDB 2.3+ and 3.x
- Used in production environments
- Active development and maintenance

**Why couch_rs**:
- ✅ Async/await support
- ✅ Type-safe API
- ✅ Supports all major CouchDB operations
- ✅ Basic authentication support
- ✅ Well-documented
- ✅ Recent updates (2024-2025)

**Alternatives Rejected**:
- `couchdb` (couchdb-rs): Last updated 2017, not async
- `chill`: Less mature, fewer features
- Direct HTTP with reqwest: Too low-level, would reinvent couch_rs

## Architecture

### Connection Model

1. HTTP-based connection (not persistent TCP)
2. URL format: `http://hostname:port` or `https://hostname:port`
3. Optional basic authentication (username/password)
4. Server info fetched on connect (GET /)
5. Operations driven by LLM actions (not event loop)

### LLM Integration

**Events**:
- `couchdb_connected` - Fired after successful connection and server info retrieval
- `couchdb_response_received` - Fired after each operation completes
- `couchdb_conflict` - Fired when document update conflict detected (409)
- `couchdb_change_detected` - Fired when changes feed detects document change

**Actions**:
- **Database operations**: create_database, delete_database, list_databases
- **Document CRUD**: create_document, get_document, update_document, delete_document
- **Bulk operations**: bulk_docs, list_documents
- **View queries**: query_view (limited - couch_rs support varies)
- **Changes feed**: watch_changes (limited - continuous mode not implemented)
- **Connection**: disconnect

### State Management

- No persistent read loop (HTTP-based, not TCP)
- Operations are one-off HTTP requests
- Client kept alive with periodic tick (5s interval)
- Status: Connecting → Connected → Disconnected
- LLM memory stores: last revision, database names, operation history

### Action Execution Flow

1. LLM generates action (e.g., `create_document`)
2. Action converted to HTTP request via couch_rs `req()` method (for document write operations) or high-level API (for database/read operations)
3. HTTP request sent to CouchDB server
4. Response received and parsed
5. `couchdb_response_received` event sent to LLM
6. LLM decides next action

### Implementation Details

**Database Operations** (uses high-level couch_rs API):
- `create_database` → `client.make_db()`
- `delete_database` → `client.destroy_db()`
- `list_databases` → `client.list_dbs()`

**Document Read Operations** (uses high-level couch_rs API):
- `get_document` → `db.get_raw()`
- `list_documents` → `db.get_all_raw()`

**Document Write Operations** (uses raw HTTP API via `client.req()`):
- `create_document` → `PUT /{db}/{docid}` or `POST /{db}` (auto-generated ID)
- `update_document` → `PUT /{db}/{docid}` (requires `_rev` in document)
- `delete_document` → `DELETE /{db}/{docid}?rev={rev}`
- `bulk_docs` → `POST /{db}/_bulk_docs` with `{"docs": [...]}`

**Reason for raw HTTP API**: couch_rs requires `TypedCouchDocument` trait for write operations, but we need to accept arbitrary JSON from the LLM. The `req()` method provides direct HTTP access that bypasses this requirement.

### Error Handling

**Conflict Detection (409)**:
- Update/delete operations require `_rev` (revision)
- If revision mismatch → 409 Conflict
- Special `couchdb_conflict` event sent to LLM
- LLM can: retry with correct rev, fetch latest doc, abort

**Other Errors**:
- Network errors (connection refused, timeout)
- Authentication errors (401)
- Not found errors (404)
- All errors reported in `couchdb_response_received` event with `success: false`

## Supported Operations

### Database Management

**create_database**:
```json
{
  "type": "create_database",
  "database": "mydb"
}
```

**delete_database**:
```json
{
  "type": "delete_database",
  "database": "mydb"
}
```

**list_databases**:
```json
{
  "type": "list_databases"
}
```

### Document CRUD

**create_document** (with ID):
```json
{
  "type": "create_document",
  "database": "mydb",
  "doc_id": "user1",
  "document": {"name": "Alice", "age": 30}
}
```

**create_document** (auto-generated ID):
```json
{
  "type": "create_document",
  "database": "mydb",
  "document": {"name": "Bob", "age": 25}
}
```

**get_document**:
```json
{
  "type": "get_document",
  "database": "mydb",
  "doc_id": "user1"
}
```

**update_document** (requires `_rev`):
```json
{
  "type": "update_document",
  "database": "mydb",
  "doc_id": "user1",
  "document": {"_rev": "1-abc123", "name": "Alice", "age": 31}
}
```

**delete_document** (requires `_rev`):
```json
{
  "type": "delete_document",
  "database": "mydb",
  "doc_id": "user1",
  "rev": "2-def456"
}
```

### Bulk Operations

**bulk_docs**:
```json
{
  "type": "bulk_docs",
  "database": "mydb",
  "docs": [
    {"_id": "doc1", "name": "Alice"},
    {"_id": "doc2", "name": "Bob"},
    {"_id": "doc3", "_deleted": true, "_rev": "1-abc"}
  ]
}
```

**list_documents**:
```json
{
  "type": "list_documents",
  "database": "mydb",
  "include_docs": false
}
```

### View Queries (Limited)

**query_view**:
```json
{
  "type": "query_view",
  "database": "mydb",
  "design_doc": "users",
  "view_name": "by_age",
  "params": {"limit": 10, "key": 25}
}
```

Note: View query support depends on couch_rs implementation. Not all features may be available.

### Changes Feed (Limited)

**watch_changes**:
```json
{
  "type": "watch_changes",
  "database": "mydb",
  "since": "now",
  "feed": "normal"
}
```

Note: Continuous changes feed not implemented. Only normal/longpoll modes partially supported.

## Revision Management

CouchDB uses MVCC with document revisions:

**Format**: `"{sequence}-{hash}"` (e.g., `"1-abc123"`, `"2-def456"`)

**LLM Responsibilities**:
- Remember `_rev` from create/update responses
- Include `_rev` in update/delete operations
- Handle conflict events (fetch latest, retry)

**Conflict Resolution**:
1. LLM attempts update with `_rev: "1-abc"`
2. Server has `_rev: "2-def"` (document changed)
3. Server returns 409 Conflict
4. Client sends `couchdb_conflict` event to LLM
5. LLM can:
   - Fetch latest doc (`get_document`)
   - Merge changes
   - Retry update with correct `_rev`

## Authentication

**Basic Auth**:
- Configured via startup parameters: `username`, `password`
- Sent with every HTTP request
- Format: `Authorization: Basic base64(username:password)`

**Example**:
```
Connect to CouchDB at localhost:5984 with username 'admin' and password 'secret'
```

Startup params: `{ "username": "admin", "password": "secret" }`

## Limitations

### Protocol Features

- **No continuous changes feed** - Only normal/longpoll partially supported
- **Limited view queries** - Depends on couch_rs support
- **No attachments** - Binary attachments not implemented
- **No Mango queries** - Only views and `_all_docs`
- **No replication** - CouchDB replication protocol not exposed
- **No cookie auth** - Only basic auth supported
- **No bulk get** - `_bulk_get` endpoint not exposed in couch_rs

### Performance

- Each operation is a separate HTTP request
- No connection pooling (handled by reqwest internally)
- No batching (except explicit bulk_docs)

### Data Management

- **LLM memory critical** - Must remember revisions, database names
- **Conflict handling** - LLM must implement merge strategy
- **No local cache** - All data fetched from server

## Known Issues

1. **View queries**: couch_rs support for views is basic - complex queries may fail
2. **Changes feed**: Continuous mode not implemented - polling only
3. **Attachments**: Not supported - would need additional implementation
4. **Error details**: Some couch_rs errors lose server error details
5. **Connection state**: No TCP state - client appears "connected" even if server down

## Example LLM Prompts

### Basic Database Operations

**Create database and add documents**:
```
Connect to CouchDB at localhost:5984.
Create a database called 'users'.
Add two documents: {name: 'Alice', age: 30} and {name: 'Bob', age: 25}.
```

LLM actions:
1. `create_database` → "users"
2. `create_document` → Alice
3. `create_document` → Bob

### Update with Conflict Resolution

**Update document with conflict handling**:
```
Connect to CouchDB at localhost:5984.
Get document 'user1' from database 'mydb'.
Update the age to 31.
If there's a conflict, fetch the latest version and try again.
```

LLM actions:
1. `get_document` → Receives doc with `_rev: "1-abc"`
2. `update_document` with `_rev: "1-abc"` → Conflict!
3. LLM receives `couchdb_conflict` event
4. `get_document` → Receives doc with `_rev: "2-def"`
5. `update_document` with `_rev: "2-def"` → Success

### Bulk Operations

**Bulk insert**:
```
Connect to CouchDB at localhost:5984.
In database 'products', insert 100 products with names "Product 1" through "Product 100".
Use bulk operations for efficiency.
```

LLM actions:
1. `bulk_docs` with array of 100 documents

## Testing

**E2E Testing**:
- Test against NetGet CouchDB server (self-testing!)
- Use mocks for LLM responses
- < 10 LLM calls per test suite

**Test Scenarios**:
1. Connect and get server info
2. Create database
3. CRUD operations (create, get, update, delete)
4. Conflict detection and resolution
5. Bulk operations
6. List databases and documents

## References

- [couch_rs Documentation](https://docs.rs/couch_rs)
- [CouchDB HTTP API](https://docs.couchdb.org/en/stable/api/index.html)
- [CouchDB Document API](https://docs.couchdb.org/en/stable/api/document/common.html)

## Injected actions (the dashboard's `[ send ]`)

The client registers a command channel and spawns `command_loop` **before** the
`couchdb_connected` LLM call, because a `*` -> manual rule can park that call for minutes
and the operator must still be able to reach the client. The command task replaces the old
5s keep-alive poll: the channel closes when the client is removed.

The command loop shares the very same `Arc<Mutex<couch_rs::Client>>` the LLM path uses and
calls the same `execute_couchdb_action`, so an injected action is indistinguishable from an
LLM-produced one - including its `couchdb_response_received` event and the follow-up actions
that event's handler returns, which the command loop drains exactly like the connect path.
The action is first round-tripped through `CouchDbClientProtocol::execute_action` so an
unknown verb is reported rather than silently ignored.

`ClientSendOutcome` semantics:

| Outcome | When |
|---|---|
| `Executed { detail }` | The operation ran, e.g. `list_databases executed` (plus a count when the response event produced follow-up actions). |
| `Rejected { error }` | The action is not one of the thirteen CouchDB verbs. |
| `Disconnected` | `{"type":"disconnect"}`; status goes to Disconnected, the loop exits, the handle is dropped. |
| `Err(...)` | The operation returned an error the executor could not turn into a response event. |

**`Sent { bytes_sent }` is never reported**: `couch_rs` owns the HTTP connection, so a byte
count would be invented.

---

## The deferred paths execute the model's answer too (August 2026)

Three sites asked the model what to do and could not act on the reply. All three are fixed.

| site | was | why it mattered |
|---|---|---|
| `send_response_event`, `Dispatch::Deferred` | counted the actions into an `info!` and dropped them | this is the path **every dashboard-injected command** takes |
| `send_conflict_event`, `Dispatch::Deferred` | `if let Err(..)` — no success arm at all | a 409 is exactly the event whose answer must run: the model has just been told a write lost a revision race and asked what to do |
| `watch_changes_feed` | `if let Err(..)` — no success arm | a client told to watch a database and react watched, and did nothing |

**The stated reason for the first one was false**, and worth recording because it reads
plausibly: *"only the client's own loop owns the CouchDB handle, and this task runs beside
it."* The handle is an `Arc<tokio::sync::Mutex<couch_rs::Client>>` — shared by construction —
and `execute_couchdb_action` already took it by reference. The inline sibling of that branch
returned its actions and they ran; the deferred path was simply the odd one out. Likewise
`watch_changes_feed` claimed "the event's answer is handled by the normal event path", and
nothing handled it.

Executing makes the cycle real (`create_database` → `couchdb_response_received` →
`create_database`), so it is **bounded, not cut**: `MAX_FOLLOWUP_DEPTH = 6`, with the limit
reported on the status stream rather than hit silently. `run_followups` is the one place the
queue is drained.

`execute_couchdb_action` returns an explicitly boxed `Pin<Box<dyn Future + Send>>` instead of
being an `async fn`. That is required, not stylistic: the chain `execute_couchdb_action` →
`send_response_event` → `run_followups` → `execute_couchdb_action` is a cycle of `async fn`s
whose opaque return types cannot be inferred (E0391), and boxing at a *call* site does not help
because the coercion still needs the callee's opaque type.

One consequence to be aware of: the deferred command loop's "N follow-up action(s) from the
response event" message could previously only ever say 0, because `Dispatch::Deferred` returned
an empty vector by construction. It now reports real numbers.
