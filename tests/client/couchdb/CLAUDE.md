# CouchDB Client E2E Testing

## Overview

E2E tests for CouchDB client implementation using NetGet CouchDB server (self-testing pattern). Tests client-server interaction with mock LLM responses.

## Test Strategy

**Approach**: Self-testing (client connects to NetGet server)
- CouchDB client connects to NetGet CouchDB server on same instance
- Mocks all LLM responses for both client and server
- Tests full request-response cycle
- Verifies client action execution and event handling

**LLM Call Budget**: 0 LLM calls (all mocked)
- All tests use `.with_mock()` pattern
- Each test verifies mock expectations with `.verify_mocks()`
- No real Ollama/LLM calls required

**Expected Runtime**: < 8 seconds for full test suite

## Test Coverage

### 1. Client Connection (`test_couchdb_client_connect`)
- Tests: Client connection and server info retrieval
- Client actions: None (initial connection)
- Server actions: `send_server_info`
- Events: `couchdb_connected`
- LLM calls: 0

### 2. Database Operations (`test_couchdb_client_database_operations`)
- Tests: Create, list, delete databases via client
- Client actions: `create_database`, `list_databases`, `delete_database`
- Server actions: `send_couchdb_response`, `send_all_dbs`
- Events: `couchdb_connected`, `couchdb_response_received` (3x)
- Flow: Connect → Create DB → List DBs → Delete DB
- LLM calls: 0

### 3. Document CRUD (`test_couchdb_client_document_crud`)
- Tests: Create, read, update, delete documents via client
- Client actions: `create_document`, `get_document`, `update_document`, `delete_document`
- Server actions: `send_doc_response` (4x)
- Events: `couchdb_connected`, `couchdb_response_received` (4x)
- Flow: Connect → Create → Get → Update → Delete
- Verifies: Revision tracking across operations
- LLM calls: 0

### 4. Conflict Handling (`test_couchdb_client_conflict_handling`)
- Tests: 409 conflict detection and LLM-driven resolution
- Client actions: `update_document` (2x), `get_document`
- Server actions: `send_doc_response` (3x - conflict, get, success)
- Events: `couchdb_connected`, `couchdb_conflict`, `couchdb_response_received` (2x)
- Flow: Connect → Update (old rev) → Conflict → Get latest → Update (correct rev) → Success
- Verifies: Conflict event triggers get+retry pattern
- LLM calls: 0

### 5. Bulk Operations (`test_couchdb_client_bulk_operations`)
- Tests: Bulk document insert and listing
- Client actions: `bulk_docs`, `list_documents`
- Server actions: `send_bulk_docs_response`, `send_all_docs`
- Events: `couchdb_connected`, `couchdb_response_received` (2x)
- Flow: Connect → Bulk insert 3 docs → List all docs
- LLM calls: 0

## Mock Patterns

**Client-Server Interaction**:
```rust
// Client event triggers action
.on_event("couchdb_connected")
.respond_with_actions(json!([{
    "type": "create_document",
    "database": "testdb",
    "doc_id": "user1",
    "document": {"name": "Alice"}
}]))

// Server receives request and responds
.on_event("couchdb_request")
.and_event_data_contains("operation", "doc_put")
.respond_with_actions(json!([{
    "type": "send_doc_response",
    "success": true,
    "doc_id": "user1",
    "rev": "1-abc123"
}]))

// Client receives response event
.on_event("couchdb_response_received")
.and_event_data_contains("operation", "create_document")
.and_event_data_contains("success", true)
.respond_with_actions(json!([...]))
```

**Timing**: Tests use `tokio::time::sleep()` to allow async operations to complete before verification.

## Known Issues

None. All tests pass reliably with mocks.

## Future Enhancements

1. **View queries** - Test `query_view` action (when implemented)
2. **Changes feed** - Test `watch_changes` action (when implemented)
3. **Authentication** - Test client with username/password startup params
4. **Error handling** - Test network failures, timeouts, invalid responses
5. **Memory updates** - Verify client memory tracking of revisions

## Running Tests

```bash
# Run all CouchDB client tests
./cargo-isolated.sh test --no-default-features --features couchdb --test client::couchdb::e2e_test

# Run specific test
./cargo-isolated.sh test --no-default-features --features couchdb --test client::couchdb::e2e_test test_couchdb_client_document_crud

# Run with parallel execution (default)
./cargo-isolated.sh test --no-default-features --features couchdb --test client::couchdb::e2e_test -- --test-threads=100
```

## Test Execution Flow Example

For `test_couchdb_client_document_crud`:

1. **Setup**: Start NetGet with CouchDB server on port X and client connecting to 127.0.0.1:X
2. **Connect**: Client connects, receives `couchdb_connected` event
3. **Create**: Mock triggers `create_document` action → Client sends PUT request → Server receives `couchdb_request` → Mock triggers `send_doc_response` → Client receives HTTP response → Client fires `couchdb_response_received` event
4. **Get**: Mock triggers `get_document` action → (similar flow)
5. **Update**: Mock triggers `update_document` action → (similar flow)
6. **Delete**: Mock triggers `delete_document` action → (similar flow)
7. **Verify**: All mock expectations met
8. **Cleanup**: Stop server

Total time: ~3 seconds

## References

- [CouchDB HTTP API Documentation](https://docs.couchdb.org/en/stable/api/index.html)
- [NetGet Test Infrastructure](../../README.md)
