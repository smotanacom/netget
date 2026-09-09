# CouchDB Protocol Implementation

## Overview

CouchDB-compatible server implementing the CouchDB HTTP/JSON REST API. The server handles database management, document CRUD, views (MapReduce), changes feed, replication, and basic authentication with full LLM control over responses. This is a "virtual" document database where the LLM maintains data and query results through conversation context.

**State**: `Experimental`, and it stays there. The only e2e evidence is `reqwest`, a generic
HTTP client, which proves an HTTP server answers — not that the CouchDB API on top of it is
right. `metadata()` claimed `couch_rs`; that is what the CouchDB *client* protocol uses and no
test has ever pointed it at this server. Promoting this means driving it with a real CouchDB
client or replicator.
**Port**: 5984 (default CouchDB port)
**Protocol**: HTTP/1.1 with JSON payloads
**API Version**: CouchDB 3.5.1 compatible
**Stack Representation**: `ETH>IP>TCP>HTTP>COUCHDB`

## Library Choices

**hyper** (v1.5):
- HTTP/1.1 server implementation
- Async connection handling with tokio
- Service-based request routing
- Handles HTTP framing, headers, body parsing

**http-body-util**:
- Body aggregation (`BodyExt::collect()`)
- Full body type (`Full<Bytes>`)
- Efficient byte handling

**serde_json**:
- JSON request/response parsing
- CouchDB uses JSON for all data
- No binary protocol support

**base64**:
- HTTP Basic Authentication credential encoding/decoding
- Format: `Basic base64(username:password)`

**form_urlencoded**:
- Query parameter parsing
- Used for view queries, changes feed parameters

**Manual API Implementation**:
- LLM controls all CouchDB operations through action system
- No CouchDB server dependencies
- Responses manually constructed as JSON
- Operation detection from HTTP method + path

## Architecture Decisions

### REST API Design

- Operations determined by HTTP method (GET, POST, PUT, DELETE, HEAD) and path
- Path structure:
  - `GET /` → Server info
  - `PUT /{db}` → Create database
  - `PUT /{db}/{docid}` → Create/update document
  - `GET /{db}/_all_docs` → List all documents
  - `GET /{db}/_design/{ddoc}/_view/{view}` → Query view
  - `GET /{db}/_changes` → Changes feed
  - `POST /_replicate` → Trigger replication
- Request body is JSON (for POST/PUT operations)
- Response body is JSON with operation results
- Standard HTTP status codes (200, 201, 400, 401, 404, 409, 500)
- Custom headers:
  - `Server: CouchDB/3.5.1 (NetGet LLM)`
  - `ETag: "{revision}"` for documents
  - `WWW-Authenticate: Basic realm="CouchDB"` for auth challenges

### Request body limit

The body is bounded at `MAX_REQUEST_BODY_BYTES` (8 MiB) with `http_body_util::Limited`, and a
larger one is refused with **413** `{"error": "too_large"}` *before* any LLM call. `Incoming`
has no default limit, so this used to buffer whatever an unauthenticated peer chose to send —
one `POST /db/_bulk_docs` was enough to exhaust the process — and the body is then embedded
whole in an LLM prompt, so there is no legitimate large one either.

The old `Err` arm fell through with an **empty** body, which is worse than the limit being
absent: the handler was shown a request with no body and answered it as though the client had
sent none, so a truncated bulk update read as an empty one.

The constant is defined locally rather than shared, because `server::http_common` is gated on
`any(feature = "http", "http2", "oauth2", …)` and `couchdb` is not in that list — the same exit
`xmlrpc` takes. Adding `couchdb` and `elasticsearch` to that gate in `src/server/mod.rs` would
let both share `http_common::MAX_REQUEST_BODY_BYTES`.

### Stateless Operation

- Each HTTP request is independent
- No persistent storage or connection state
- LLM maintains "virtual" databases and documents through conversation context
- Server ID used but no per-connection state
- Revision management entirely in LLM memory

### Request Processing Flow

1. Accept TCP connection
2. Parse HTTP request (method, URI, headers, body)
3. Check authentication if enabled (HTTP Basic Auth)
4. Detect operation from method + path (e.g., PUT + "/{db}/{docid}" → doc_put)
5. Parse database name, document ID, and query parameters from path
6. Create `COUCHDB_REQUEST_EVENT` with method, path, operation, body
7. Call LLM via `call_llm()` (which first tries any script/static handler, so those cost no
   model call)
8. Process action result:
   - `couchdb_response`: Build HTTP response with status/body/etag
9. If no action, return **500** `{"error": "no_response", ...}` — never a 2xx. A 200
   `{"ok": true}` is the claim that the document was written (or, on `_session`, that the
   login succeeded), and nothing here is in a position to make it. `no_response` is a
   distinct `error` string from the backend-failure branch's `internal_server_error`, so a
   silent handler and a dead backend stay distinguishable in the client's error and the log
10. Keep the connection open for further requests

### Operation Detection

#### Server Operations
- **Server info** (`GET /`) → server_info
- **All databases** (`GET /_all_dbs`) → all_dbs
- **Active tasks** (`GET /_active_tasks`) → active_tasks
- **UUIDs** (`GET /_uuids`) → uuids
- **Replicate** (`POST /_replicate`) → replicate
- **Session** (`GET /_session`) → session

#### Database Operations
- **Create database** (`PUT /{db}`) → db_create
- **Delete database** (`DELETE /{db}`) → db_delete
- **Database info** (`GET /{db}`) → db_info

#### Document Operations
- **Create document** (`POST /{db}`) → doc_create (auto-generated ID)
- **Get document** (`GET /{db}/{docid}`) → doc_get
- **Create/update document** (`PUT /{db}/{docid}`) → doc_put
- **Delete document** (`DELETE /{db}/{docid}`) → doc_delete
- **Head document** (`HEAD /{db}/{docid}`) → doc_head

#### Bulk Operations
- **All documents** (`GET /{db}/_all_docs`) → all_docs
- **Bulk documents** (`POST /{db}/_bulk_docs`) → bulk_docs
- **Bulk get** (`POST /{db}/_bulk_get`) → bulk_get

#### View Operations
- **Get design doc** (`GET /{db}/_design/{ddoc}`) → design_get
- **Create design doc** (`PUT /{db}/_design/{ddoc}`) → design_put
- **Delete design doc** (`DELETE /{db}/_design/{ddoc}`) → design_delete
- **Query view** (`GET|POST /{db}/_design/{ddoc}/_view/{view}`) → view_query

#### Changes Feed
- **Changes** (`GET /{db}/_changes`) → changes
  - Query params: `feed=normal|longpoll|continuous`, `since={seq}`

#### Replication Protocol
- **Local docs** (`GET|PUT /{db}/_local/{docid}`) → local_doc_get/put
- **Revs diff** (`POST /{db}/_revs_diff`) → revs_diff

#### Attachment Operations
- **Get attachment** (`GET /{db}/{docid}/{attachment}`) → attachment_get
- **Put attachment** (`PUT /{db}/{docid}/{attachment}`) → attachment_put
- **Delete attachment** (`DELETE /{db}/{docid}/{attachment}`) → attachment_delete

### Response Format

- Status codes:
  - 200 (success), 201 (created), 204 (no content)
  - 400 (bad request), 401 (unauthorized), 403 (forbidden)
  - 404 (not found), 409 (conflict - revision mismatch)
  - 500 (internal server error)
- Headers:
  - `Content-Type: application/json`
  - `Server: CouchDB/3.5.1 (NetGet LLM)`
  - `ETag: "{revision}"` (for document responses)
- Body: JSON object with operation-specific fields
- Error format: `{"error": "conflict", "reason": "Document update conflict"}`

### Revision Management (MVCC)

CouchDB uses Multi-Version Concurrency Control (MVCC) with revisions:

- **Revision Format**: `"{sequence}-{hash}"` (e.g., `"1-abc123"`, `"2-def456"`)
- **LLM generates revisions**: Sequence increments on each update, hash is random
- **Conflict detection**: LLM validates `_rev` parameter on updates
  - If `_rev` doesn't match current → 409 Conflict error
- **ETag header**: Document revision sent as `ETag: "1-abc123"`
- **If-Match header**: Client can send expected revision for conditional updates

### Basic Authentication

HTTP Basic Authentication support:

- **Configuration**: `enable_auth`, `admin_username`, `admin_password` startup params
- **Header format**: `Authorization: Basic base64(username:password)`
- **Validation**: Base64 decode and compare credentials
- **Challenges**: `WWW-Authenticate: Basic realm="CouchDB"` on 401
- **LLM awareness**: the event carries `authorization` as the literal string `"***"` when a
  header was present and `null` when it was not — deliberately redacted, so the LLM can see
  *that* credentials were sent but cannot see them and cannot validate them. Basic-auth
  enforcement happens in `handle_couchdb_request_with_llm` before the LLM is consulted; when
  `enable_auth` is false every request is served unconditionally. `send_auth_required` lets
  the LLM issue its own 401 challenge on top of that

### Changes Feed

Real-time document change notifications:

- **Feed types**:
  - `normal`: Return all changes since sequence, close connection
  - `longpoll`: Wait for changes, return when available
  - `continuous`: Keep connection open, stream changes (LLM simulates)
- **Parameters**: `since={seq}`, `limit={n}`, `include_docs={true|false}`
- **Response**: Array of `{seq, id, changes: [{rev}]}`
- **LLM tracking**: LLM maintains change log in conversation memory

### View Queries (MapReduce)

Simplified view query support:

- **Design documents**: Stored as special `_design/{ddoc}` documents
- **View definition**: JSON with `map` function (JavaScript code as string)
- **LLM execution**: LLM interprets map function, computes results from document memory
- **Query parameters**: `key`, `startkey`, `endkey`, `limit`, `skip`, `include_docs`
- **Response**: `{total_rows, offset, rows: [{id, key, value}]}`
- **Limitation**: No real JavaScript engine - LLM simulates map function execution

### Replication Protocol

Simplified replication support:

- **Local documents**: `_local/{docid}` for checkpoint tracking
- **Revs diff**: `POST /{db}/_revs_diff` to find missing revisions
- **Bulk get**: `POST /{db}/_bulk_get` to fetch multiple documents
- **LLM simulation**: LLM tracks replication state, simulates missing rev detection
- **Limitation**: Not full CouchDB replication protocol - simplified for LLM control

## LLM Integration

### Action-Based Responses

**Sync Actions** (network event context required):
- `send_couchdb_response`: Generic response — `status_code` (required, rejected outside
  100-599), `body` (required), optional `etag`
- `send_server_info`: Server welcome/version info (GET /)
- `send_db_info`: Database information (doc count, update_seq)
- `send_doc_response`: Document response (GET/PUT/POST/DELETE) with revision. Set
  `"created": true` on a PUT/POST that stored a document so the reply is 201 Created, as
  real CouchDB answers; reads and deletes stay 200
- `send_all_dbs`: List of all databases
- `send_all_docs`: List of all documents in database. `total_rows` is **required** and
  enforced — a defaulted 0 beside a non-empty `rows` array is a response no client can
  reconcile, and both replicators and Fauxton read it
- `send_bulk_docs_response`: Bulk operation results
- `send_view_response`: View query results (MapReduce). `total_rows` **required**, as above
- `send_changes_response`: Changes feed response. `last_seq` is **required** and enforced: it
  is the checkpoint a replicator stores and resumes from, so the old default of `"0"` told the
  peer it had seen nothing immediately after handing it changes — which makes it restart from
  the beginning of the database on every pass, forever
- `send_replication_response`: Replication protocol response. `session_id` and
  `source_last_seq` are **required**. Every parameter used to be optional with a default
  supplied, so a bare `{"type": "send_replication_response"}` produced `{"ok": true}` — the
  most affirmative body the replication protocol has — with the literal session id `"abc123"`
  and `source_last_seq: "0"`, for a replication nothing performed. That is the OAuth2
  fail-open shape, and the peer stores `source_last_seq` as a checkpoint
- `send_auth_required`: 401 Unauthorized challenge

**Event Types**:
- `COUCHDB_REQUEST_EVENT`: Fired for every CouchDB operation
  - Data: `{ "method": "PUT", "path": "/{db}/{docid}", "operation": "doc_put", "database": "mydb", "doc_id": "user1", "query_params": {...}, "request_body": "{...}" }`

### Example LLM Prompts

**Server info**:
```
For GET / request, use send_server_info with version="3.5.1"
```

**Create database**:
```
For PUT /mydb, use send_couchdb_response with:
status_code=201
body='{"ok": true}'
```

**Create document**:
```
For PUT /mydb/user1 with body {"name": "Alice"}, use send_doc_response with:
success=true
created=true
doc_id="user1"
rev="1-abc123"
Remember this document in mydb: user1 = {"name": "Alice", "_id": "user1", "_rev": "1-abc123"}
```

**Get document**:
```
For GET /mydb/user1, retrieve from memory and use send_doc_response with:
success=true
doc_id="user1"
rev="1-abc123"
document={"name": "Alice", "age": 30}
```

**Update document with conflict**:
```
For PUT /mydb/user1 with _rev="1-old", check stored revision.
If mismatch, use send_doc_response with:
success=false
doc_id="user1"
rev="2-current"
error="conflict"
reason="Document update conflict"
```

**Bulk documents**:
```
For POST /mydb/_bulk_docs with array of docs, use send_bulk_docs_response with:
results=[
  {"ok": true, "id": "doc1", "rev": "1-abc"},
  {"ok": true, "id": "doc2", "rev": "1-def"}
]
```

**View query**:
```
For GET /mydb/_design/users/_view/by_age, execute map function from memory and use send_view_response with:
total_rows=2
rows=[
  {"id": "user1", "key": 25, "value": "Alice"},
  {"id": "user2", "key": 30, "value": "Bob"}
]
```

**Changes feed**:
```
For GET /mydb/_changes?since=0, return all changes from memory and use send_changes_response with:
results=[
  {"seq": "1-abc", "id": "doc1", "changes": [{"rev": "1-xyz"}]},
  {"seq": "2-def", "id": "doc2", "changes": [{"rev": "1-uvw"}]}
]
last_seq="2-def"
```

**Authentication required**:
```
For any request without valid auth (when auth enabled), use send_auth_required with realm="CouchDB"
```

## Connection Management

### Connection Lifecycle

1. Server accepts TCP connection on port 5984
2. Create `ConnectionId` for tracking
3. Add connection to `ServerInstance` with `ProtocolConnectionInfo::empty()` (that type is a
   generic `serde_json::Value` wrapper, not a per-protocol enum)
4. Spawn HTTP service handler
5. `http1::Builder::serve_connection` serves the connection, including keep-alive
6. Connection closed when the client closes it

### State Tracking

- Connection state stored in `ServerInstance.connections` HashMap
- No protocol-specific connection state is recorded
- Tracks: remote_addr, local_addr. `bytes_sent`/`bytes_received` are initialised to 0 and
  never updated — `update_connection_stats` is not called, so the rail's `↓ ↑` counters stay
  at zero for a live connection
- Status: Active → Closed when the connection ends

### Concurrency

- Multiple connections handled concurrently
- Each connection is independent (stateless HTTP)
- No shared state between connections
- LLM maintains "virtual" databases and documents through conversation memory

## Limitations

### Protocol Features

- **No persistent storage** - data only exists in LLM conversation context
- **Simplified authentication** - basic auth only, no cookie auth or JWT
- **HTTP/1.1 only** - no HTTP/2 support
- **No streaming** - full request/response buffering; a `continuous` changes feed is
  answered as a single body, not streamed
- **Simplified views** - LLM simulates MapReduce, no real JavaScript engine
- **No reduce functions** - only map functions in views
- **No attachment actions** - `GET|PUT|DELETE /{db}/{docid}/{attachment}` *are* detected and
  reach the LLM as `attachment_get`/`attachment_put`/`attachment_delete`, but there is no
  action that returns binary attachment content. The only reply available is
  `send_couchdb_response` with a JSON body, so attachment requests cannot be served
  faithfully
- **No Mango queries** - only views and `_all_docs`
- **Simplified replication** - not full CouchDB replication protocol
- **No Fauxton** - no web UI (CouchDB admin interface)
- **No partitioned databases** - single partition only
- **No purge action** - `POST /{db}/_purge` is detected and reaches the LLM as `purge`, but
  has no dedicated action; answer it with `send_couchdb_response`

### Performance

- Each request triggers LLM call
- No actual B-tree indexing or MapReduce
- Full request/response in memory
- Connection overhead per request

### Data Management

- **Virtual data** - LLM maintains databases/documents through conversation
- **No persistence** - data lost when LLM context is cleared
- **Consistency** - depends on LLM memory
- **Scalability** - limited by LLM context window
- **Revision generation** - LLM creates revision IDs, not cryptographic hashes
- **Conflict resolution** - LLM detects conflicts but doesn't support multi-master

## Known Issues

1. **Data consistency**: LLM may forget or hallucinate documents between requests
2. **Complex views**: Advanced MapReduce functions may confuse LLM
3. **Large responses**: Very large result sets may exceed response size limits
4. **Continuous changes**: Streaming changes feed requires long-lived connections
5. **Replication fidelity**: Not 100% compatible with CouchDB replication protocol
6. **Revision hashes**: Not cryptographic - LLM generates pseudo-random strings
7. **Attachments**: routed to the LLM but unanswerable — see Limitations
8. **Basic auth comparison** is a plain `==` on the decoded credentials, not constant-time

## Example Responses

### Server Info (GET /)

```json
{
  "couchdb": "Welcome",
  "version": "3.5.1",
  "git_sha": "netget-llm",
  "uuid": "netget-couchdb-uuid",
  "features": ["access-ready", "partitioned", "pluggable-storage-engines"],
  "vendor": {
    "name": "NetGet LLM CouchDB"
  }
}
```

### Create Document Success

```json
{
  "ok": true,
  "id": "user1",
  "rev": "1-abc123"
}
```

### Get Document

```json
{
  "_id": "user1",
  "_rev": "2-def456",
  "name": "Alice",
  "age": 30
}
```

### Conflict Error (409)

```json
{
  "error": "conflict",
  "reason": "Document update conflict"
}
```

### All Databases

```json
["_replicator", "_users", "mydb", "testdb"]
```

### All Docs

```json
{
  "total_rows": 2,
  "offset": 0,
  "rows": [
    {"id": "doc1", "key": "doc1", "value": {"rev": "1-abc"}},
    {"id": "doc2", "key": "doc2", "value": {"rev": "1-def"}}
  ]
}
```

### View Query

```json
{
  "total_rows": 2,
  "offset": 0,
  "rows": [
    {"id": "user1", "key": 25, "value": "Alice"},
    {"id": "user2", "key": 30, "value": "Bob"}
  ]
}
```

### Changes Feed

```json
{
  "results": [
    {"seq": "1-abc", "id": "doc1", "changes": [{"rev": "1-xyz"}]},
    {"seq": "2-def", "id": "doc2", "changes": [{"rev": "1-uvw"}]}
  ],
  "last_seq": "2-def",
  "pending": 0
}
```

## References

- [CouchDB API Reference](https://docs.couchdb.org/en/stable/api/index.html)
- [CouchDB Replication Protocol](https://docs.couchdb.org/en/stable/replication/protocol.html)
- [CouchDB HTTP API](https://docs.couchdb.org/en/stable/intro/api.html)
- [CouchDB Views](https://docs.couchdb.org/en/stable/ddocs/views/index.html)
- [CouchDB Changes Feed](https://docs.couchdb.org/en/stable/api/database/changes.html)
