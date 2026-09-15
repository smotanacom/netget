# CouchDB Server E2E Testing

## Overview

E2E tests for CouchDB server implementation using real HTTP clients (reqwest) and mock LLM responses.

## Test Strategy

**Approach**: Black-box testing with HTTP client
- Uses reqwest to make real HTTP requests
- Mocks all LLM responses for fast, deterministic tests
- Tests CouchDB HTTP API compliance
- Verifies JSON response formats

**LLM Call Budget**: 0 LLM calls (all mocked)
- All tests use `.with_mock()` pattern
- Each test verifies mock expectations with `.verify_mocks()`
- No real Ollama/LLM calls required

**Expected Runtime**: < 5 seconds for full test suite

## Test Coverage

### 1. Server Info (`test_couchdb_server_info`)
- Tests: `GET /` endpoint
- Verifies: Server welcome message, version info
- Mocks: `send_server_info` action
- LLM calls: 0

### 2. Database Operations (`test_couchdb_database_operations`)
- Tests: Create, info, delete database
- Endpoints: `PUT /{db}`, `GET /{db}`, `DELETE /{db}`
- Verifies: HTTP status codes, response format
- Mocks: `send_couchdb_response`, `send_db_info`
- LLM calls: 0

### 3. Document CRUD (`test_couchdb_document_crud`)
- Tests: Create, read, update, delete documents
- Endpoints: `PUT /{db}/{docid}`, `GET /{db}/{docid}`, `DELETE /{db}/{docid}`
- Verifies: Document revisions, JSON data persistence
- Mocks: `send_doc_response`
- LLM calls: 0

### 4. Conflict Detection (`test_couchdb_conflict_detection`)
- Tests: 409 Conflict on revision mismatch
- Endpoint: `PUT /{db}/{docid}` with old `_rev`
- Verifies: HTTP 409 status, error message format
- Mocks: `send_doc_response` with `success: false`
- LLM calls: 0

### 5. Bulk Operations (`test_couchdb_bulk_operations`)
- Tests: Bulk docs insert, all docs listing
- Endpoints: `POST /{db}/_bulk_docs`, `GET /{db}/_all_docs`
- Verifies: `201 Created` for `_bulk_docs` (as real CouchDB answers), array responses, result counts
- Mocks: `send_bulk_docs_response`, `send_all_docs`
- LLM calls: 0

### 6. View Queries (`test_couchdb_view_query`)
- Tests: MapReduce view query
- Endpoint: `GET /{db}/_design/{ddoc}/_view/{view}`
- Verifies: View result format (rows, keys, values)
- Mocks: `send_view_response`
- LLM calls: 0

### 7. Basic Authentication (`test_couchdb_basic_auth`)
- Tests: HTTP Basic Auth challenge and success
- Endpoint: `GET /` with no auth header, with wrong credentials, and with valid credentials
- Verifies: 401 + `WWW-Authenticate: Basic` twice, then 200 OK
- Starts the server with `startup_params: {enable_auth, admin_username, admin_password}` —
  without them `AuthConfig::enabled` is false and the test proves nothing about auth
- Mocks: `send_server_info` only. Auth is enforced in `handle_couchdb_request_with_llm`
  **before** `call_llm`, so an unauthenticated request never reaches the model. Only the
  authenticated request produces an LLM call, and its rule is keyed on
  `authorization == "***"` to say so
- LLM calls: 0 (mocked); 1 mock rule invocation

### 8. Changes Feed (`test_couchdb_changes_feed`)
- Tests: Document change notifications
- Endpoint: `GET /{db}/_changes`
- Verifies: Change sequence format, last_seq
- Mocks: `send_changes_response`
- LLM calls: 0

## Mock Patterns

**Event Matching**:
```rust
.on_event("couchdb_request")
.and_event_data_contains("operation", "doc_put")
.and_event_data_contains("doc_id", "user1")
```

**Action Response**:
```rust
.respond_with_actions(json!([{
    "type": "send_doc_response",
    "success": true,
    "doc_id": "user1",
    "rev": "1-abc123"
}]))
```

**Verification**:
```rust
server.verify_mocks().await?;  // Ensures all expected calls happened
```

## Known Issues

**Two mock rules with identical matchers are a silent trap.** Rules are evaluated in
declaration order and the *first* match answers, so a suite that declares one rule per
request — rather than one rule per distinguishable request — has the first rule answer
every time and the later one never fire. `test_couchdb_document_crud` (create vs. update
`PUT /testdb/user1`) and `test_couchdb_basic_auth` (unauthenticated vs. authenticated
`GET /`) both had this shape and both failed only at `verify_mocks()`, long after the wrong
response had already gone out on the wire. Give every rule something that distinguishes it:
`request_body` contains `_rev` for the update, `authorization` for the authenticated request.

## Future Enhancements

1. **Replication testing** - Test `POST /_replicate` endpoint
2. **Attachment testing** - Test binary attachment upload/download
3. **Mango queries** - Test `POST /{db}/_find` endpoint (if implemented)
4. **Continuous changes** - Test `feed=continuous` mode (if implemented)
5. **Design doc CRUD** - Test `PUT /{db}/_design/{ddoc}`

## Running Tests

```bash
# Run all CouchDB server tests
./cargo-isolated.sh test --no-default-features --features couchdb --test server::couchdb::e2e_test

# Run specific test
./cargo-isolated.sh test --no-default-features --features couchdb --test server::couchdb::e2e_test test_couchdb_document_crud
```

## References

- [CouchDB HTTP API Documentation](https://docs.couchdb.org/en/stable/api/index.html)
- [NetGet Test Infrastructure](../../README.md)
