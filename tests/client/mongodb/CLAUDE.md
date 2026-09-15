# MongoDB Client E2E Testing

## Testing Strategy

**Black-box, mock-driven testing** where the netget MongoDB client connects to a real MongoDB server (or mock) and LLM controls the operations.

### Test Philosophy

1. **Mock-first**: All tests use `.with_mock()` builder pattern
2. **LLM call budget**: < 10 calls per test suite
3. **Real server**: Use local MongoDB server (via Docker or system install)
4. **Feature-gated**: `#[cfg(all(test, feature = "mongodb"))]`
5. **Client-server pairing**: Some tests use both server and client

## Test Suite Structure

### Test Files

- `tests/client/mongodb/e2e_test.rs` - Main E2E tests with mocks
- `tests/client/mongodb/CLAUDE.md` - This file (test strategy)

### Test Coverage

**Core Functionality** (5 tests):
1. `test_mongodb_client_find_with_mocks` - Query with filter
2. `test_mongodb_client_insert_with_mocks` - Insert document
3. `test_mongodb_client_update_with_mocks` - Update documents
4. `test_mongodb_client_delete_with_mocks` - Delete documents
5. `test_mongodb_client_workflow_with_mocks` - Multi-step operations

**Server-Client Integration** (1 test):
6. `test_mongodb_server_and_client_with_mocks` - Full integration

**Total LLM calls**: 6-12 (1-2 per test)

## Mock Patterns

### Client Find Mock

```rust
let client_config = NetGetConfig::new(format!(
    "Connect to MongoDB at 127.0.0.1:{} database testdb. \
     Find all users with age greater than 25.",
    server.port
))
.with_mock(|mock| {
    mock
        .on_event("mongodb_connected")
        .respond_with_actions(serde_json::json!([
            {
                "type": "find_documents",
                "collection": "users",
                "filter": {"age": {"$gt": 25}}
            }
        ]))
        .expect_calls(1)
        .and()
        .on_event("mongodb_result_received")
        .and_event_data_contains("result_type", "find")
        .respond_with_actions(serde_json::json!([
            {"type": "disconnect"}
        ]))
        .expect_calls(1)
        .and()
});

let client = start_netget_client(client_config).await?;
```

### Client Insert Mock

```rust
.with_mock(|mock| {
    mock
        .on_event("mongodb_connected")
        .respond_with_actions(serde_json::json!([
            {
                "type": "insert_document",
                "collection": "users",
                "document": {"name": "Charlie", "age": 35}
            }
        ]))
        .expect_calls(1)
        .and()
        .on_event("mongodb_result_received")
        .and_event_data_contains("result_type", "insert")
        .respond_with_actions(serde_json::json!([
            {"type": "disconnect"}
        ]))
        .expect_calls(1)
        .and()
})
```

### Client Update Mock

```rust
.with_mock(|mock| {
    mock
        .on_event("mongodb_connected")
        .respond_with_actions(serde_json::json!([
            {
                "type": "update_documents",
                "collection": "users",
                "filter": {"name": "Alice"},
                "update": {"$set": {"age": 31}}
            }
        ]))
        .expect_calls(1)
        .and()
})
```

### Client Delete Mock

```rust
.with_mock(|mock| {
    mock
        .on_event("mongodb_connected")
        .respond_with_actions(serde_json::json!([
            {
                "type": "delete_documents",
                "collection": "users",
                "filter": {"age": {"$lt": 18}}
            }
        ]))
        .expect_calls(1)
        .and()
})
```

### Multi-Step Workflow Mock

```rust
.with_mock(|mock| {
    mock
        // Step 1: Connected → find low-stock products
        .on_event("mongodb_connected")
        .respond_with_actions(serde_json::json!([
            {
                "type": "find_documents",
                "collection": "products",
                "filter": {"stock": {"$lt": 10}}
            }
        ]))
        .expect_calls(1)
        .and()
        // Step 2: Result received → update products
        .on_event("mongodb_result_received")
        .and_event_data_contains("result_type", "find")
        .respond_with_actions(serde_json::json!([
            {
                "type": "update_documents",
                "collection": "products",
                "filter": {"stock": {"$lt": 10}},
                "update": {"$inc": {"reorder_count": 1}}
            }
        ]))
        .expect_calls(1)
        .and()
        // Step 3: Update result → insert notification
        .on_event("mongodb_result_received")
        .and_event_data_contains("result_type", "update")
        .respond_with_actions(serde_json::json!([
            {
                "type": "insert_document",
                "collection": "notifications",
                "document": {"type": "reorder_alert", "timestamp": "2025-01-01T00:00:00Z"}
            }
        ]))
        .expect_calls(1)
        .and()
        // Step 4: Insert complete → disconnect
        .on_event("mongodb_result_received")
        .and_event_data_contains("result_type", "insert")
        .respond_with_actions(serde_json::json!([
            {"type": "disconnect"}
        ]))
        .expect_calls(1)
        .and()
})
```

## Server Setup

### Option 1: Real MongoDB Server (Docker)

```bash
# Start MongoDB in Docker
docker run -d -p 27017:27017 --name mongodb-test mongo:latest

# Or use docker-compose
services:
  mongodb:
    image: mongo:latest
    ports:
      - "27017:27017"
```

### Option 2: NetGet MongoDB Server

```rust
// Start netget MongoDB server for testing
let server_config = NetGetConfig::new(
    "Listen on port {AVAILABLE_PORT} via MongoDB"
)
.with_mock(|mock| {
    mock
        .on_event("mongodb_command")
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

let server = start_netget_server(server_config).await?;

// Connect netget client to netget server
let client_config = NetGetConfig::new(format!(
    "Connect to MongoDB at 127.0.0.1:{} database testdb",
    server.port
))
.with_mock(|mock| { /* client mocks */ });

let client = start_netget_client(client_config).await?;
```

## Verification Strategy

### Mock Verification (CRITICAL)

**Always verify both server and client mocks:**

```rust
#[tokio::test]
async fn test_mongodb_server_and_client_with_mocks() -> E2EResult<()> {
    let server = start_netget_server(server_config).await?;
    let client = start_netget_client(client_config).await?;

    // Wait for operations to complete
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    server.verify_mocks().await?;  // ← Server mocks
    client.verify_mocks().await?;  // ← Client mocks
    Ok(())
}
```

### Operation Verification

Since the client is LLM-controlled, verification happens through:

1. **Mock expectations**: LLM called correct number of times
2. **Event data**: LLM received correct result data
3. **Server state**: Server received expected commands (in server-client tests)

```rust
// Verify via mock expectations
mock
    .on_event("mongodb_result_received")
    .and_event_data_contains("documents", /* some array */)
    .expect_calls(1)
```

## Test Runtime

**Expected runtime per test**: < 2 seconds (mocked)

**Total suite runtime**: < 15 seconds

- Mock mode: Fast (no Ollama calls, no real queries)
- Real Ollama + Real MongoDB: Moderate (15-60 seconds)

## Known Issues

### Connection Timeout

If client can't connect to MongoDB:
```rust
// Check MongoDB server is running
docker ps | grep mongo

// Check port availability
netstat -an | grep 27017

// Add timeout in test
tokio::time::timeout(
    Duration::from_secs(5),
    start_netget_client(config)
).await??;
```

### Database Cleanup

Tests should use unique database names to avoid conflicts:
```rust
let db_name = format!("test_db_{}", uuid::Uuid::new_v4());
let client_config = NetGetConfig::new(format!(
    "Connect to MongoDB at 127.0.0.1:27017 database {}",
    db_name
));
```

### BSON Conversion Errors

If LLM returns invalid BSON structure:
```rust
// Mock should return valid MongoDB documents
{
    "documents": [
        {"_id": {"$oid": "..."}, "field": "value"}  // ✅ Valid
        // NOT: {"_id": "plain_string"}  // ❌ Invalid for BSON
    ]
}
```

## Debugging Failed Tests

### Enable Client Logging

```bash
RUST_LOG=netget::client::mongodb=trace,mongodb=debug cargo test --features mongodb
```

### Check MongoDB Server Logs

```bash
# Docker
docker logs mongodb-test

# System MongoDB
tail -f /var/log/mongodb/mongod.log
```

### Inspect Client State

```rust
// In test code
println!("Client status: {:?}", client.status);
println!("Client connection: {:?}", client.connection);
```

## Example Test

```rust
#[cfg(all(test, feature = "mongodb"))]
mod mongodb_client_tests {
    use super::*;

    #[tokio::test]
    async fn test_mongodb_client_find_with_mocks() -> E2EResult<()> {
        // Start netget MongoDB server
        let server_config = NetGetConfig::new(
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
                            {"name": "Alice", "age": 30}
                        ]
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let server = start_netget_server(server_config).await?;

        // Start netget MongoDB client
        let client_config = NetGetConfig::new(format!(
            "Connect to MongoDB at 127.0.0.1:{} database testdb. \
             Find all users.",
            server.port
        ))
        .with_mock(|mock| {
            mock
                .on_event("mongodb_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "find_documents",
                        "collection": "users",
                        "filter": {}
                    }
                ]))
                .expect_calls(1)
                .and()
                .on_event("mongodb_result_received")
                .and_event_data_contains("result_type", "find")
                .respond_with_actions(serde_json::json!([
                    {"type": "disconnect"}
                ]))
                .expect_calls(1)
                .and()
        });

        let client = start_netget_client(client_config).await?;

        // Wait for operations
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Verify mocks
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        Ok(())
    }
}
```

## Running Tests

### Run MongoDB Client Tests Only

```bash
# With mocks (fast)
./test-e2e.sh mongodb

# With real Ollama (slow)
./test-e2e.sh --use-ollama mongodb

# With cargo (parallel)
cargo test --features mongodb --test client::mongodb::e2e_test -- --test-threads=100
```

### Run With Real MongoDB Server

```bash
# Start MongoDB
docker run -d -p 27017:27017 --name mongodb-test mongo:latest

# Run tests
cargo test --features mongodb --test client::mongodb::e2e_test

# Cleanup
docker stop mongodb-test && docker rm mongodb-test
```

## CI/CD Integration

```yaml
# .github/workflows/test.yml
- name: Start MongoDB
  run: docker run -d -p 27017:27017 mongo:latest

- name: Test MongoDB Client
  run: |
    ./cargo-isolated.sh test --no-default-features --features mongodb \
      --test client::mongodb::e2e_test -- --test-threads=100

- name: Stop MongoDB
  run: docker stop $(docker ps -q --filter ancestor=mongo:latest)
```

## References

- [MongoDB Rust Driver Testing](https://github.com/mongodb/mongo-rust-driver/tree/main/tests)
- [NetGet Client Testing Patterns](../mysql/CLAUDE.md)
- [NetGet Test Infrastructure](../../README.md)
