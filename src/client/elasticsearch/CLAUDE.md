# Elasticsearch Client Implementation

## Overview

The Elasticsearch client provides LLM-controlled access to Elasticsearch clusters via the Elasticsearch REST API over
HTTP. It supports the core operations: indexing, searching, document retrieval, deletion, and bulk operations.

## Library Choices

### HTTP Client: `reqwest`

- **Rationale**: Elasticsearch uses a REST API over HTTP/HTTPS, making a general HTTP client ideal
- **Version**: 0.12+ with JSON support
- **Benefits**:
    - Async/await support via Tokio
    - Built-in TLS support for HTTPS
    - JSON serialization/deserialization
    - Well-maintained and widely used

### No Official Elasticsearch Crate

The official `elasticsearch` Rust crate exists but adds unnecessary complexity for LLM-controlled operations. Instead,
we directly use `reqwest` to make HTTP calls to the Elasticsearch REST API, giving the LLM full control over request
construction.

## Architecture

### Connection Model

Elasticsearch is **HTTP-based**, so "connection" is a logical concept:

1. **Initialization**: Client stores the cluster URL (e.g., `http://localhost:9200`)
2. **Stateless Requests**: Each operation creates a new HTTP request
3. **No Persistent Connection**: Unlike TCP-based protocols, there's no long-lived socket

### LLM Integration

The LLM controls Elasticsearch operations through structured actions:

```rust
// LLM Action Flow:
// 1. User opens Elasticsearch client
// 2. LLM receives "elasticsearch_connected" event
// 3. LLM constructs operations (index, search, etc.)
// 4. Client executes HTTP requests to Elasticsearch
// 5. LLM receives "elasticsearch_response_received" event
// 6. LLM analyzes response and decides next action
```

### State Management

Client state stored in `protocol_data`:

- `es_client`: Initialization marker
- `cluster_url`: Base URL for Elasticsearch cluster
- `authorization`: `Basic <base64(username:password)>`, resolved once at connect from the
  `username` / `password` startup parameters. Every request goes through
  `cluster_endpoint` + `authorize`, so authentication cannot be applied to some operations
  and forgotten on others. A username with no password is a valid Basic credential
  (`user:`), so the username alone switches authentication on.
- `default_index`: the `default_index` startup parameter. `resolve_index` uses it whenever an
  operation names no index; `bulk_operation` is exempt, because `/_bulk` is cluster-wide and
  each entry names its own index.

### Request Construction

Each operation builds the appropriate HTTP request:

**Index Document**: `POST /{index}/_doc/{id}` or `POST /{index}/_doc`

```json
{
  "name": "John Doe",
  "email": "john@example.com"
}
```

**Search**: `POST /{index}/_search`

```json
{
  "query": {
    "match": {
      "name": "John"
    }
  }
}
```

**Get Document**: `GET /{index}/_doc/{id}`

**Delete Document**: `DELETE /{index}/_doc/{id}`

**Bulk Operations**: `POST /_bulk` with NDJSON format

```
{"index":{"_index":"users","_id":"1"}}
{"name":"Alice"}
{"delete":{"_index":"users","_id":"2"}}
```

## LLM Control Points

### Async Actions (User-Triggered)

1. **index_document**: Index a document
    - Parameters: `index` (string), `id` (optional string), `document` (object)
    - LLM constructs JSON document structure

2. **search**: Search documents
    - Parameters: `index` (string), `query` (object)
    - LLM constructs Elasticsearch Query DSL

3. **get_document**: Retrieve document by ID
    - Parameters: `index` (string), `id` (string)

4. **delete_document**: Delete document by ID
    - Parameters: `index` (string), `id` (string)

5. **bulk_operation**: Execute multiple operations
    - Parameters: `operations` (array of operation objects)
    - LLM constructs bulk operation sequence

6. **disconnect**: Close client

### Sync Actions (Response-Triggered)

A client's tool list is async ∪ sync ∪ the firing event's actions, so all five operation
verbs are offered on `elasticsearch_response_received` and all five are dispatched:

1. **index_document**: Index follow-up documents based on search results
2. **search**: Perform additional searches based on results
3. **get_document** / **delete_document**: act on a specific hit
4. **bulk_operation**: batch follow-up work in one `_bulk` request

### Event Types

1. **elasticsearch_connected**
    - Triggered when client initializes
    - Parameters: `cluster_url`
    - LLM decides initial operation

2. **elasticsearch_response_received**
    - Triggered after each operation
    - Parameters: `operation`, `status_code`, `response` — note these are the **top-level**
      fields. `result`, `found`, `hits` and `items` live *inside* `response`, so an
      `event_handlers` rule or a test mock keyed on them at the top level never matches.
    - LLM analyzes response and decides next action

    Follow-up operations report their own results too, bounded by
    `MAX_FOLLOWUP_DEPTH` (4). They used to run and report nothing, on the stated
    reasoning that "responses don't trigger new operations" — so the chain was exactly one
    step deep and a `search` issued in reply to an index confirmation sent its hits
    nowhere. The model could not read the results of a search it had itself asked for.
    The recursion this avoided is real (report → action → report), so the call is boxed;
    the depth bound is what actually keeps it finite.

    **Every** verb the model is offered on this event has a dispatch arm: `search`,
    `get_document`, `delete_document`, `index_document` and `bulk_operation`. The last was
    missing and logged as "skipped", so the one verb that batches work was the one the
    model could not use in reply to a result — an advertised verb with no arm is the same
    defect one level down. Its NDJSON body comes from `build_bulk_ndjson`, shared with the
    direct path, and travels as `RequestBody::Ndjson`: Elasticsearch refuses a `_bulk`
    request whose content type says `application/json`.

## Logging Strategy

### Dual Logging

All operations use **dual logging** (tracing macros + `status_tx`):

```rust
info!("Elasticsearch client {} searching index {}", client_id, index);
let _ = status_tx.send(format!("[CLIENT] Searching index {}", index));
```

### Log Levels

- **INFO**: Operation lifecycle (index, search, delete)
- **DEBUG**: Request details (would be added for debugging)
- **ERROR**: Operation failures
- **TRACE**: Full request/response bodies (not currently implemented)

## Example Prompts

### Basic Indexing

```
"Connect to http://localhost:9200 and index a document in the 'users' index with fields name='John Doe' and age=30"
```

### Search Query

```
"Search the 'logs' index for all documents with level='error' in the last hour"
```

### Bulk Operations

```
"Index 3 documents into the 'products' index: laptop, phone, and tablet with their prices"
```

### Complex Query

```
"Search 'articles' index for documents matching 'machine learning' in the title, filter by author='Smith', and sort by publish_date descending"
```

## Limitations

### 1. No Connection Pooling

Each operation creates a new `reqwest::Client`. For production use, connection pooling would improve performance.

**Workaround**: Client lifecycle is short-lived, so this has minimal impact.

### 2. Basic Auth Only

`username` / `password` produce an `Authorization: Basic ...` header on every request. API
keys and bearer tokens have no parameter; there is no place to put one short of a new
startup parameter that is actually read.

### 3. Query DSL Complexity

LLM must understand Elasticsearch Query DSL to construct effective searches.

**Mitigation**: Provide clear examples in action definitions and rely on LLM's training data.

### 4. No Streaming Search Results

Large result sets are returned all at once, not streamed.

**Workaround**: Use pagination parameters (`from`, `size`) in search queries.

### 5. Bulk Operation Error Handling

Bulk operations may partially succeed. Individual operation errors are in the response but not parsed separately.

**Mitigation**: LLM should check the `errors` field in bulk response.

### 6. No Index Management

No operations for creating/deleting indices, managing mappings, or cluster admin.

**Future Enhancement**: Add `create_index`, `delete_index`, `put_mapping` actions.

### 7. HTTP-Only

Current implementation uses `http://` by default. HTTPS requires client modification.

**Workaround**: User can specify `https://` in the remote address.

## Error Handling

### Connection Errors

If Elasticsearch cluster is unreachable:

```rust
status_code: 0  // reqwest error, no HTTP response
response: {"error": "Failed to send request"}
```

### Elasticsearch Errors

Elasticsearch returns errors in standard format:

```json
{
  "error": {
    "type": "index_not_found_exception",
    "reason": "no such index [missing]"
  },
  "status": 404
}
```

LLM receives this in the `elasticsearch_response_received` event and can react accordingly.

## Testing Notes

See `tests/client/elasticsearch/CLAUDE.md` for E2E test strategy.

## Future Enhancements

1. **Authentication**: API keys and bearer tokens (Basic Auth already works)
2. **Index Management**: Create/delete indices, mappings
3. **Aggregations**: More complex analytics
4. **Scroll API**: For large result sets
5. **Multi-Index Search**: Search across multiple indices
6. **Cluster Health**: Operations to check cluster status
7. **Connection Pooling**: Reuse HTTP client across operations
8. **HTTPS Support**: Automatic HTTPS for secure clusters

## Implementation Complexity

**Rating**: Medium (🟡)

**Justification**:

- HTTP-based API is straightforward
- JSON request/response handling is simple
- No complex protocol state machine
- Main complexity is in Query DSL construction (handled by LLM)
- Bulk operations require NDJSON formatting

## Dependencies

- `reqwest`: HTTP client with JSON support
- No additional Elasticsearch-specific crates needed

## Command channel (dashboard `[ send ]`)

`AppState::send_to_client` can inject an action into a running Elasticsearch client.
`connect_with_llm_actions` registers the channel (`command_support::register_command_channel`)
**before** the connected-event LLM call and spawns a registered `command_loop` task; the loop
replaces the old 5s "has this client been removed yet" poll.

Injected actions go through `ElasticsearchClient::apply_action`, which the connected-event path
now also uses. That fixed a second bug on the way: the connect path dispatched only
`index_document` and `search`, so `get_document`, `delete_document` and `bulk_operation` were
advertised to the model and silently dropped. Every operation returns the HTTP status code
rather than `()` so the outcome can report what the cluster actually said.

Outcome semantics — `reqwest` frames and sends the request, so there is no honest wire byte
count and **`Sent` is never returned**:

| Situation | `ClientSendOutcome` |
|---|---|
| `execute_action` refused it | `Rejected { error }` |
| Request completed | `Executed { detail: "search completed: HTTP 200" }` |
| Request failed (transport, or a missing required field) | `Executed { detail: "search failed: …" }` |
| `disconnect` | `Disconnected` (loop ends, handle removed) |

The HTTP request itself is **awaited** in the loop, so the reported status is real. The
`elasticsearch_response_received` event is raised from its own registered task
(`spawn_response_notification`), so an event handler that parks for a human answer cannot wedge
the command loop. The connect path additionally *spawns* `apply_action` itself (registered as a
client task) so `connect` returns promptly.

Test: `tests/client/elasticsearch/command_channel_test.rs` (no LLM, no cluster — a loopback
listener is the endpoint).
