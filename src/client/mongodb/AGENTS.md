# MongoDB Client Implementation

## Overview

This MongoDB client implementation provides LLM-controlled access to MongoDB servers using the official `mongodb` Rust driver. The client connects to real MongoDB servers and allows the LLM to execute queries, inserts, updates, and deletes.

## Architecture

### Library: Official MongoDB Driver

**`mongodb` v3.3** - Official async MongoDB driver
- Full wire protocol support (OP_MSG)
- Connection pooling
- TLS support
- Async/await with Tokio
- BSON serialization via `bson` v3.0

**Why `mongodb` crate?**
- **Official**: Maintained by MongoDB team
- **Complete**: Full MongoDB 4.2+ feature support
- **Production-ready**: Used in production Rust applications
- **Async-first**: Native Tokio integration
- **Well-documented**: Comprehensive API docs

### Connection Model

**Query-Response** (like MySQL/PostgreSQL clients):
```
User Instruction
    ↓
open_client → connect_with_llm_actions()
    ↓
Parse connection string (mongodb://host:port/db)
    ↓
Connect to MongoDB server
    ↓
Get Database handle (Arc<Database>)
    ↓
Call LLM with mongodb_connected Event
    ↓
Execute Actions from LLM
    ├─ find_documents → Collection::find()
    ├─ insert_document → Collection::insert_one()
    ├─ update_documents → Collection::update_many()
    ├─ delete_documents → Collection::delete_many()
    └─ disconnect → Update client status
    ↓
For each result:
    ├─ Convert BSON → JSON
    ├─ Send mongodb_result_received Event to LLM
    └─ Execute follow-up actions — which report their results too, up to
       MAX_FOLLOWUP_DEPTH (4)
```

**The chain used to stop after one hop.** Follow-ups ran through `apply_action`
directly, which executes but raises no event, under a comment arguing that was a
deliberate bound. It is the recorded `elasticsearch` client defect: a `find` the
model issued in reply to an insert confirmation ran and returned its documents to
nobody. The honest form of the same protection is a depth bound — every operation
reports, the recursion is boxed (an `async fn` awaiting itself has an
infinitely-sized future), and past depth 4 an action still executes but its result
is not reported, so nothing recurses further. The cap is logged at WARN and on the
status stream, never silently.

**Maturity**: `Experimental`. The e2e tests point this client at NetGet's *own*
MongoDB server, so the official driver is the subject rather than the corroborating
peer; nothing has run it against a real `mongod`. Both test files are therefore
gated on `all(feature = "mongodb", feature = "mongodb-server")` — `e2e_test.rs` was
gated on `mongodb` alone and its four tests failed outright on that feature set.

### State Management

**Synchronous execution** (one operation at a time):
```rust
let db_arc = Arc::new(db);  // Shared database reference

// No Mutex needed - mongodb crate handles internal synchronization
let collection = db.collection::<Document>("users");
let cursor = collection.find(filter).await?;
let documents: Vec<Document> = cursor.collect().await;
```

## LLM Integration

### Events

**1. mongodb_connected** - Fired after successful connection
```json
{
  "remote_addr": "localhost:27017",
  "database": "testdb"
}
```

**2. mongodb_result_received** - Fired after operation completes
```json
{
  "result_type": "find",
  "documents": [
    {"_id": {"$oid": "..."}, "name": "Alice", "age": 30}
  ]
}
```
Or for modifications:
```json
{
  "result_type": "update",
  "count": 2
}
```

### Actions

**1. find_documents** - Query documents
```json
{
  "type": "find_documents",
  "collection": "users",
  "filter": {"age": {"$gte": 18}},
  "projection": {"name": 1, "age": 1},
  "limit": 10
}
```

**2. insert_document** - Insert a document
```json
{
  "type": "insert_document",
  "collection": "users",
  "document": {"name": "Charlie", "age": 35, "email": "charlie@example.com"}
}
```

**3. update_documents** - Update documents
```json
{
  "type": "update_documents",
  "collection": "users",
  "filter": {"name": "Alice"},
  "update": {"$set": {"age": 31}, "$inc": {"login_count": 1}}
}
```

**4. delete_documents** - Delete documents
```json
{
  "type": "delete_documents",
  "collection": "users",
  "filter": {"age": {"$lt": 18}}
}
```

**5. disconnect** - Close connection
```json
{
  "type": "disconnect"
}
```

**6. wait_for_more** - Wait without action
```json
{
  "type": "wait_for_more"
}
```

## Data Handling

### BSON ↔ JSON Conversion

**JSON to BSON** (for queries):
```rust
use bson::{doc, to_document, Document};

// Filter from LLM (JSON)
let filter_json = json!({"age": {"$gte": 18}});

// Convert to BSON Document
let filter: Document = bson::to_document(&filter_json)?;

// Use in query
collection.find(filter).await?;
```

**BSON to JSON** (for LLM results):
```rust
// MongoDB returns Vec<Document>
let documents: Vec<Document> = cursor.collect().await;

// Convert to JSON for LLM
let json_docs: Vec<serde_json::Value> = documents
    .iter()
    .filter_map(|doc| bson::to_bson(doc).ok())
    .filter_map(|bson| bson.into_canonical_extjson().as_document().cloned())
    .filter_map(|doc| bson::from_document(doc).ok())
    .collect();
```

### Structured Data Approach

**CRITICAL**: All MongoDB operations use structured JSON, never binary BSON in actions.

```rust
// ✅ GOOD: Structured filter
{
  "filter": {"age": {"$gte": 18}, "active": true}
}

// ❌ BAD: Binary BSON (never used)
{
  "filter_bytes": "EwAAAAJhZ2UAAw..." // Base64 BSON
}
```

## Connection Lifecycle

### 1. Connection String Parsing
```rust
// With authentication
let connection_string = format!("mongodb://{}:{}@{}", user, pass, remote_addr);

// Without authentication
let connection_string = format!("mongodb://{}", remote_addr);

// Parse options
let client_options = ClientOptions::parse(&connection_string).await?;
```

### 2. Client Creation
```rust
let mongo_client = MongoClient::with_options(client_options)?;
let db = mongo_client.database(&database_name);
```

### 3. Shared Database Access
```rust
let db_arc = Arc::new(db);  // Share across tasks

// No Mutex needed - mongodb crate is thread-safe
let collection = db.collection::<Document>("users");
```

### 4. Operation Execution
```rust
// Find
let cursor = collection.find(filter).with_options(find_options).await?;
let docs: Vec<Document> = cursor.collect().await;

// Insert
let result = collection.insert_one(document).await?;

// Update
let result = collection.update_many(filter, update).await?;

// Delete
let result = collection.delete_many(filter).await?;
```

## Startup Parameters

**database** (optional, default: "admin"):
```json
{
  "database": "myapp"
}
```

**username** (optional):
```json
{
  "username": "myuser"
}
```

**password** (optional):
```json
{
  "password": "secret123"
}
```

**Example**:
```
Connect to MongoDB at localhost:27017 database ecommerce as admin with password secret
```

## Error Handling

MongoDB errors are logged and reported to the LLM:
```rust
match collection.find(filter).await {
    Ok(cursor) => { /* process results */ }
    Err(e) => {
        error!("MongoDB find error: {}", e);
        // LLM receives error context in next interaction
    }
}
```

Common errors:
- **Authentication failed**: Wrong username/password
- **Namespace not found**: Collection doesn't exist
- **Connection timeout**: Server unreachable
- **Query error**: Invalid filter syntax

## Example Prompts

### Basic Query
```
Connect to MongoDB at localhost:27017 database testdb.
Find all users with age greater than 25.
```

### Multi-Step Workflow
```
Connect to MongoDB at 127.0.0.1:27017 database ecommerce.

1. Find all products with stock less than 10
2. For each low-stock product, increment the reorder_count
3. Insert a reorder notification document
```

### Aggregation-Style (via Multiple Queries)
```
Connect to MongoDB at localhost:27017 database analytics.

Find all orders from the last 30 days.
Group them by customer_id (manually in conversation).
Calculate total spending per customer.
Find the top 5 spenders.
```

## Limitations

1. **No Direct Aggregation Pipeline**: Use multiple queries + LLM logic
2. **No GridFS**: File storage not supported
3. **No Change Streams**: No real-time updates
4. **No Transactions**: Single-operation only
5. **No Connection Pooling Control**: Uses mongodb crate defaults
6. **No Write Concern**: Uses default write concern
7. **No Read Preference**: Uses primary by default

## Performance Characteristics

- **Connection overhead**: Moderate (TLS handshake, authentication)
- **Query latency**: Low (direct driver, no intermediate layers)
- **LLM latency**: High (1-3 seconds per decision)
- **Throughput**: Limited by LLM, not MongoDB
- **Memory**: Efficient (streaming cursors for large result sets)

## Testing Approach

See `tests/client/mongodb/CLAUDE.md` for E2E testing strategy.

## Future Enhancements

**Phase 2**:
- Aggregation pipeline support (complex LLM instructions)
- Index creation/management
- Write concern configuration

**Phase 3**:
- GridFS file operations
- Change stream monitoring
- Transaction support (multi-document ACID)

## Implementation Notes

### Why Official mongodb Crate?

Alternatives considered:
1. **`mongodb` crate**: ✅ Chosen - official, complete, well-maintained
2. **Direct TCP + manual OP_MSG**: Too complex, reinventing the wheel
3. **HTTP REST API**: Not standard MongoDB protocol
4. **`polodb`**: Embedded database, not a client

### Thread Safety

The `mongodb` crate's `Database` and `Collection` types are:
- **`Send + Sync`**: Safe to share across threads
- **Thread-safe**: Internal synchronization handles concurrent access
- **Arc-friendly**: No Mutex needed for sharing

```rust
// ✅ Safe without Mutex
let db_arc = Arc::new(db);
tokio::spawn(async move {
    let collection = db_arc.collection("users");
    collection.find(doc!{}).await?;
});
```

### Memory Management

**Streaming Cursors** prevent loading all results into memory:
```rust
use futures::stream::StreamExt;

let cursor = collection.find(filter).await?;

// Stream results one at a time
while let Some(doc) = cursor.next().await {
    let doc = doc?;
    // Process document
}

// Or collect (for LLM integration)
let docs: Vec<Document> = cursor.collect().await;
```

### Action Execution Pattern

```rust
match protocol.execute_action(action)? {
    ClientActionResult::Custom { name: "mongodb_find", data } => {
        let collection_name = data["collection"].as_str()?;
        let filter = bson::to_document(&data["filter"])?;

        let collection = db.collection::<Document>(collection_name);
        let cursor = collection.find(filter).await?;
        let documents = cursor.collect().await;

        // Send result event to LLM
        send_result_event(client_id, "find", Some(documents), ...).await?;
    }
    // ... other actions
}
```

## Debugging

### Enable Trace Logging
```bash
RUST_LOG=netget::client::mongodb=trace,mongodb=debug cargo run
```

### Monitor Queries
```rust
trace!("MongoDB client {} executing find on {}", client_id, collection_name);
trace!("Filter: {:?}", filter);
trace!("Result: {} documents", documents.len());
```

### Connection Diagnostics
```bash
# Test connection manually
mongosh "mongodb://localhost:27017/testdb"

# Check server logs
tail -f /var/log/mongodb/mongod.log
```

## References

- [MongoDB Rust Driver Docs](https://docs.rs/mongodb/latest/mongodb/)
- [MongoDB Rust Driver Tutorial](https://www.mongodb.com/docs/drivers/rust/)
- [BSON Rust Crate](https://docs.rs/bson/latest/bson/)
- [MongoDB Manual](https://www.mongodb.com/docs/manual/)

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` can inject an action into a running MongoDB client. The channel is
registered with `command_support::register_command_channel` **before** the `mongodb_connected`
LLM call and drained by its own task (`command_loop`), registered through
`register_client_task`. Registering first is what makes `[ send ]` usable while a manual `*`
routing rule has the connect event parked waiting for a human.

Both the LLM path and injected commands go through one `apply_action`, so an injected
`find_documents` / `insert_document` / `update_documents` / `delete_documents` does exactly
what the model's own would, and the `mongodb_result_received` event fires either way. On the
injected path that event is raised **after** the outcome is replied, so `[ send ]` is not held
for an LLM round-trip.

**Outcome semantics** (`ClientSendOutcome`):

| Situation | Outcome |
|---|---|
| find / insert / update / delete ran | `Executed { detail }` — the collection and the document count |
| a result with no wire effect (`wait_for_more`, unhandled custom) | `Executed { detail }` naming which |
| unknown action type or bad parameters | `Rejected { error }` |
| `disconnect` | `Disconnected`, then the loop ends and the handle is dropped |
| the driver returned an error (BSON conversion, server error) | `Err` — `send_to_client` returns the error |

**Never `Sent`.** The `mongodb` driver owns the socket and its connection pool, so NetGet
cannot know how many bytes an operation put on the wire. Every loop exit calls
`remove_client_handle`.

Test: `tests/client/mongodb/command_channel_test.rs`. It needs a server to talk to, and the
MongoDB **server** is behind its own `mongodb-server` feature, so the test is gated on
`all(feature = "mongodb", feature = "mongodb-server")` and compiles to nothing under
`--features mongodb` alone.
