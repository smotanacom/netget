# MongoDB Server E2E Testing

## Testing Strategy

**Black-box, mock-driven testing** using the official MongoDB Rust client to connect to the netget MongoDB server implementation.

### Test Philosophy

1. **Mock-first**: All tests use `.with_mock()` builder pattern
2. **LLM call budget**: < 10 calls per test suite
3. **Real client**: Use `mongodb` crate (same as production clients)
4. **No storage verification**: Server is stateless, verify responses only
5. **Feature-gated**: `#[cfg(all(test, feature = "mongodb-server"))]`

## Test Suite Structure

### Test Files

- `tests/server/mongodb/e2e_test.rs` - Main E2E tests with mocks
- `tests/server/mongodb/connection_bounds_test.rs` - the read deadlines, from a raw socket
- `tests/server/mongodb/CLAUDE.md` - This file (test strategy)

### `connection_bounds_test.rs` — deadlines, not answers

Zero LLM calls: the LLM endpoint is a dead port and the server is built with
`instruction: Some(String::new())`, so the model is genuinely never consulted. It speaks raw
OP_MSG rather than driving the `mongodb` crate, so it is gated on `mongodb-server` alone.

Three cases, and the third is the one that stops a lazy fix:

| Case | Asserts |
|---|---|
| peer connects and says nothing | closed at `FIRST_HEADER_READ_TIMEOUT`, having written nothing (MongoDB has no greeting) |
| peer sends eight bytes of a sixteen-byte header | closed too — `BODY_READ_TIMEOUT` never covered a *partial* header, because it arms only downstream of a complete one |
| peer completes the `hello` handshake, then goes quiet past 30 s | **not** closed: an answered session gets `IDLE_BETWEEN_MESSAGES_TIMEOUT`, because a driver's pooled connection is idle for minutes by design |

Remove the `tokio::time::timeout` around the header read and the first two hang until their own
60 s assertion windows expire. Collapse the pair onto one short number and the third fails.
Each waits generously against the 30 s bound so an ordinary scheduling delay under
`--test-threads=100` is not mistaken for a missing deadline; what is asserted is that the
connection ends *at all*, and that it did not end far too early.

### Test Coverage

**Core Functionality** (6 tests):
1. `test_mongodb_find_with_mocks` - Query with filter
2. `test_mongodb_insert_with_mocks` - Insert document
3. `test_mongodb_update_with_mocks` - Update documents
4. `test_mongodb_delete_with_mocks` - Delete documents
5. `test_mongodb_error_with_mocks` - Error responses
6. `test_mongodb_multiple_queries_with_mocks` - Sequential queries

**Total LLM calls**: 6 (one per test, reusing server instances where possible)

## Mock Patterns

### Basic Find Query Mock

```rust
let config = NetGetConfig::new(
    "Listen on port {AVAILABLE_PORT} via MongoDB. \
     When queried for users, return Alice (age 30) and Bob (age 25)."
)
.with_mock(|mock| {
    mock
        .on_event("mongodb_command")
        .and_event_data_contains("command", "find")
        .and_event_data_contains("collection", "users")
        .respond_with_actions(serde_json::json!([
            {
                "type": "find_response",
                "documents": [
                    {"_id": {"$oid": "507f1f77bcf86cd799439011"}, "name": "Alice", "age": 30},
                    {"_id": {"$oid": "507f191e810c19729de860ea"}, "name": "Bob", "age": 25}
                ]
            }
        ]))
        .expect_calls(1)
        .and()
});

let server = start_netget_server(config).await?;
```

### Insert Mock

```rust
.with_mock(|mock| {
    mock
        .on_event("mongodb_command")
        .and_event_data_contains("command", "insert")
        .respond_with_actions(serde_json::json!([
            {
                "type": "insert_response",
                "inserted_count": 1
            }
        ]))
        .expect_calls(1)
        .and()
})
```

### Update Mock

```rust
.with_mock(|mock| {
    mock
        .on_event("mongodb_command")
        .and_event_data_contains("command", "update")
        .respond_with_actions(serde_json::json!([
            {
                "type": "update_response",
                "matched_count": 2,
                "modified_count": 2
            }
        ]))
        .expect_calls(1)
        .and()
})
```

### Delete Mock

```rust
.with_mock(|mock| {
    mock
        .on_event("mongodb_command")
        .and_event_data_contains("command", "delete")
        .respond_with_actions(serde_json::json!([
            {
                "type": "delete_response",
                "deleted_count": 3
            }
        ]))
        .expect_calls(1)
        .and()
})
```

### Error Response Mock

```rust
.with_mock(|mock| {
    mock
        .on_event("mongodb_command")
        .and_event_data_contains("collection", "nonexistent")
        .respond_with_actions(serde_json::json!([
            {
                "type": "error_response",
                "code": 26,
                "message": "Namespace not found"
            }
        ]))
        .expect_calls(1)
        .and()
})
```

## Client Setup

### MongoDB Client Configuration

```rust
use mongodb::{
    bson::{doc, Document},
    options::ClientOptions,
    Client,
};

// Connect to netget MongoDB server
let uri = format!("mongodb://127.0.0.1:{}", server.port);
let client_options = ClientOptions::parse(&uri).await?;
let client = Client::with_options(client_options)?;
let db = client.database("testdb");
```

### Query Execution

```rust
// Find query
let collection = db.collection::<Document>("users");
let filter = doc! {"age": {"$gte": 25}};
let cursor = collection.find(filter).await?;
let documents: Vec<Document> = cursor.collect().await?;

// Insert
let doc = doc! {"name": "Charlie", "age": 35};
collection.insert_one(doc).await?;

// Update
let filter = doc! {"name": "Alice"};
let update = doc! {"$set": {"age": 31}};
collection.update_many(filter, update).await?;

// Delete
let filter = doc! {"age": {"$lt": 18}};
collection.delete_many(filter).await?;
```

## Verification Strategy

### Mock Verification (CRITICAL)

**Always call `.verify_mocks().await?` before test ends:**

```rust
#[tokio::test]
async fn test_mongodb_find_with_mocks() -> E2EResult<()> {
    let config = NetGetConfig::new("...").with_mock(|mock| {
        mock.on_event("mongodb_command").expect_calls(1).and()
    });

    let server = start_netget_server(config).await?;

    // Execute MongoDB queries...

    server.verify_mocks().await?;  // ← CRITICAL
    Ok(())
}
```

### Response Verification

```rust
// Verify document count
assert_eq!(documents.len(), 2);

// Verify document content
assert_eq!(documents[0].get_str("name").unwrap(), "Alice");
assert_eq!(documents[0].get_i32("age").unwrap(), 30);

// Verify insert result
let insert_result = collection.insert_one(doc).await?;
assert!(insert_result.inserted_id.as_object_id().is_some());

// Verify update result
let update_result = collection.update_many(filter, update).await?;
assert_eq!(update_result.matched_count, 2);
assert_eq!(update_result.modified_count, 2);

// Verify delete result
let delete_result = collection.delete_many(filter).await?;
assert_eq!(delete_result.deleted_count, 3);
```

## Test Runtime

**Expected runtime per test**: < 1 second (mocked)

**Total suite runtime**: < 10 seconds

- Mock mode: Fast (no Ollama calls)
- Real Ollama mode: Slower (6-30 seconds depending on model)

## Known Issues

### BSON ObjectId Generation

MongoDB clients auto-generate `_id` fields if not provided. Server mocks should:
- Return documents with valid ObjectId format: `{"$oid": "507f1f77bcf86cd799439011"}`
- Or let client generate IDs (don't include `_id` in mocks)

### Connection Timeout

If tests hang:
```rust
// Add timeout to client options
let mut client_options = ClientOptions::parse(&uri).await?;
client_options.server_selection_timeout = Some(Duration::from_secs(5));
```

### Cursor Iteration

Always consume cursors completely or they may leak:
```rust
// ✅ GOOD: Collect all documents
let documents: Vec<Document> = cursor.collect().await?;

// ✅ GOOD: Iterate and drop
while let Some(doc) = cursor.next().await {
    let doc = doc?;
    // process
}

// ❌ BAD: Partial iteration (may hang)
let first = cursor.next().await?.unwrap();
// cursor not consumed
```

## Debugging Failed Tests

### Enable MongoDB Wire Protocol Logging

```bash
RUST_LOG=netget::server::mongodb=trace,mongodb=debug cargo test --features mongodb-server
```

### Check Mock Expectations

```bash
# Run specific test with output
cargo test --features mongodb-server test_mongodb_find_with_mocks -- --nocapture

# Look for:
# - "Mock expectation met" or "Mock expectation failed"
# - "mongodb_command event received"
# - "find_response action executed"
```

### Inspect BSON Documents

```rust
// In test code
println!("Documents received: {:#?}", documents);
println!("BSON representation: {:?}", bson::to_bson(&documents[0]));
```

## Example Test

```rust
#[cfg(all(test, feature = "mongodb-server"))]
mod mongodb_server_tests {
    use super::*;
    use mongodb::{bson::doc, Client};

    #[tokio::test]
    async fn test_mongodb_find_with_mocks() -> E2EResult<()> {
        let config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via MongoDB"
        )
        .with_mock(|mock| {
            mock
                .on_event("mongodb_command")
                .and_event_data_contains("command", "find")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "find_response",
                        "documents": [
                            {"name": "Alice", "age": 30},
                            {"name": "Bob", "age": 25}
                        ]
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let server = start_netget_server(config).await?;

        // Connect MongoDB client
        let uri = format!("mongodb://127.0.0.1:{}", server.port);
        let client = Client::with_uri_str(&uri).await?;
        let db = client.database("testdb");
        let collection = db.collection::<mongodb::bson::Document>("users");

        // Execute find
        let cursor = collection.find(doc! {}).await?;
        let documents: Vec<_> = cursor.try_collect().await?;

        // Verify
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].get_str("name")?, "Alice");

        server.verify_mocks().await?;
        Ok(())
    }
}
```

## Running Tests

### Run MongoDB Server Tests Only

```bash
# With mocks (fast)
./test-e2e.sh mongodb-server

# With real Ollama (slow)
./test-e2e.sh --use-ollama mongodb-server

# With cargo (parallel)
cargo test --features mongodb-server --test server::mongodb::e2e_test -- --test-threads=100
```

### Run All Database Tests

```bash
cargo test --features mysql,postgresql,redis,mongodb-server,cassandra -- --test-threads=100
```

## CI/CD Integration

```yaml
# .github/workflows/test.yml
- name: Test MongoDB Server
  run: |
    ./cargo-isolated.sh test --no-default-features --features mongodb-server \
      --test server::mongodb::e2e_test -- --test-threads=100
```

## References

- [MongoDB Rust Driver Testing](https://github.com/mongodb/mongo-rust-driver/tree/main/tests)
- [BSON Test Utilities](https://docs.rs/bson/latest/bson/)
- [NetGet Test Infrastructure](../../README.md)

## The second client: `mongosh`

`real_client_test.rs::test_mongodb_find_against_mongosh` drives MongoDB's own shell — JavaScript
on the Node.js driver — against this server. It is **not** `#[ignore]`d and it **fails** rather
than skipping when mongosh is absent. The Rust `mongodb` driver was already independent of this
server, so this is a second independent client rather than a first, and that is the point: the
root `CLAUDE.md`'s Stable bar asks for two because one client can agree with one bug.

The session is a handshake plus `db.users.find({})`, and the assertion is on the documents the
**Node driver decoded** out of the cursor batch — name and age, per document, parsed back out of
the JSON the shell printed. A count alone would not catch a BSON field that encoded wrongly.

Two things are load-bearing:

- **`--apiVersion 1` is not decoration.** Per the MongoDB handshake spec a driver opens with a
  legacy `hello` over **OP_QUERY** unless `serverApi` or `loadBalanced` is set, and this server
  accepts only OP_MSG (2013) — an OP_QUERY gets the connection closed with nothing written, which
  surfaces as a topology error rather than as anything naming the opcode. Setting `serverApi`
  forces OP_MSG. `?loadBalanced=true` would do the same. This is a genuine limitation of the
  server and is now stated in `metadata()` rather than worked around silently.
- **A catch-all `mongodb_command` rule, declared last.** A real shell also sends `buildInfo`,
  `getParameter`, `connectionStatus`, `atlasVersion`, `getLog`, `ping` and `endSessions`, and an
  unanswered command comes back `{ok: 0, code: 59}`, which the shell reports as an error. The
  action vocabulary has no "arbitrary reply document" verb, so an empty `find_response` —
  `{ok: 1, cursor: {id: 0, firstBatch: []}}` — is the only shape that says `ok: 1` with no data.
  It is declared **after** the `find`-on-`users` rule because rules are first-match-wins and it
  would otherwise swallow the assertion.

Unproven: authentication (none is implemented), OP_COMPRESSED, OP_MSG section kind 1 document
sequences (so bulk writes), getMore/killCursors (every cursor id is 0), transactions, change
streams, and replica-set or sharded topology discovery.
