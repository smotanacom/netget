# DynamoDB Protocol Implementation

## Overview

DynamoDB-compatible server implementing the AWS DynamoDB HTTP/JSON API. The server handles DynamoDB operations (GetItem,
PutItem, Query, etc.) with full LLM control over responses. This is a "virtual" database where the LLM maintains data
through conversation context rather than persistent storage.

**Port**: 8000 (default DynamoDB local port)
**Protocol**: HTTP/1.1 with JSON payloads
**API Version**: DynamoDB_20120810
**Stack Representation**: `ETH>IP>TCP>HTTP>DYNAMODB`

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
- DynamoDB uses JSON for all data
- No binary protocol support

**Manual API Implementation**:

- LLM controls all DynamoDB operations through action system
- No AWS SDK dependencies
- Responses manually constructed as JSON
- Request ID generation (timestamp-based)

## Architecture Decisions

### HTTP-Based Design

- Each DynamoDB request is a POST to the root endpoint
- Operation specified in `x-amz-target` header (e.g., "DynamoDB_20120810.GetItem")
- Request body is JSON with table name and parameters
- Response body is JSON with operation results
- Standard HTTP status codes (200, 400, 500)

### Stateless Operation

- Each HTTP request is independent
- No persistent storage or connection state
- LLM maintains "virtual" data through conversation context
- Server ID used but no per-connection state

### Request Processing Flow

1. Accept TCP connection
2. Parse HTTP request (method, URI, headers, body)
3. Extract operation from `x-amz-target` header
4. Parse table name from JSON body
5. Create `DYNAMO_REQUEST_EVENT` with operation, table, body
6. Call LLM via `call_llm()` with event and protocol
7. Process action result:
    - `ActionResult::Custom { name: "dynamo_response", .. }`: Build HTTP response with
      status/body
8. If no action, **fail closed**: 500 `{"__type":"InternalServerError", …}` carrying the
   `WireFailure` category. It used to return `{}`, which *is* the documented success body for
   PutItem and DeleteItem — so a declined write looked performed — and reads as "no such item"
   for GetItem.
9. Close connection (HTTP/1.1 without keep-alive)

### Operation Detection

- Operations parsed from `x-amz-target` header
- Format: `DynamoDB_20120810.<Operation>`
- Supported operations: GetItem, PutItem, Query, Scan, CreateTable, DeleteTable, UpdateItem, DeleteItem, BatchGetItem,
  BatchWriteItem
- LLM decides how to respond to each operation

### Response Format

- Status: 200 (success), 400 (client error), 500 (server error). `send_dynamo_response`
  rejects any `status_code` outside 100-599 rather than truncating it.
- Headers:
    - `Content-Type: application/x-amz-json-1.0`
    - `x-amzn-RequestId: <hex-timestamp>`
- Body: JSON object with operation-specific fields
- Error format: `{"__type": "ErrorType", "message": "error message"}`

## LLM Integration

### Action-Based Responses

**Sync Actions** (network event context required):

- `send_dynamo_response` — parameters `status_code` (number, required) and `body` (string,
  required, a JSON document). This is the only protocol-specific action. `dynamo_response`
  is the *internal* `ActionResult::Custom` name the server matches on; it is not an action
  name the model may emit.

The generic actions (`show_message`, memory operations, …) are supplied centrally by
`get_network_event_common_actions()` and are available here too.

**Event Types**:

- `DYNAMO_REQUEST_EVENT`: Fired for every DynamoDB operation
    - Data: `{ "operation": "GetItem", "table_name": "Users", "request_body": "{...}" }`

### Example LLM Prompts

**GetItem operation**:

```
For GetItem on Users table with key {id: "user-123"}, use send_dynamo_response with:
status_code=200
body='{"Item":{"id":{"S":"user-123"},"name":{"S":"Alice"},"email":{"S":"alice@example.com"}}}'
```

**PutItem operation**:

```
For PutItem on Users table, use send_dynamo_response with:
status_code=200
body='{}'
```

**Query operation**:

```
For Query on Users table, use send_dynamo_response with:
status_code=200
body='{"Items":[{"id":{"S":"user-123"},"name":{"S":"Alice"}}],"Count":1,"ScannedCount":1}'
```

**Error responses**:

```
For invalid operations, use send_dynamo_response with:
status_code=400
body='{"__type":"ResourceNotFoundException","message":"Table not found"}'
```

## Connection Management

### Connection Lifecycle

1. Server accepts TCP connection on port 8000
2. Create `ConnectionId` for tracking
3. Add connection to `ServerInstance` with `ProtocolConnectionInfo::empty()` (that type is a
   generic `serde_json::Value` wrapper, not a per-protocol enum)
4. Spawn HTTP service handler
5. `http1::Builder::serve_connection` serves the connection, including keep-alive: a client
   may send several requests over one connection
6. Connection closed when the client closes it

### State Tracking

- Connection state stored in `ServerInstance.connections` HashMap
- No protocol-specific connection state is recorded (`ProtocolConnectionInfo::empty()`);
  there is no `recent_operations` list
- Tracks: remote_addr, local_addr. `bytes_sent`/`bytes_received` are initialised to 0 and
  never updated
- Status: Active → Closed when the connection ends

### Concurrency

- Multiple connections handled concurrently
- Each connection is independent (stateless HTTP)
- No shared state between connections
- LLM maintains "virtual" data through conversation memory

## Limitations

### Protocol Features

- **No persistent storage** - data only exists in LLM conversation context
- **No authentication** - AWS signature verification not implemented
- **HTTP/1.1 only** - no HTTP/2 support
- **No streaming** - full request/response buffering
- **Limited operations** - only common CRUD operations supported
- **No transactions** - no atomic multi-item operations
- **No TTL** - time-to-live not supported
- **No streams** - DynamoDB Streams not implemented
- **No global tables** - single-region only

### Performance

- Each request triggers LLM call
- No query optimization or indexing
- Full request/response in memory
- Connection overhead per request

### Data Management

- **Virtual data** - LLM maintains data through conversation
- **No persistence** - data lost when LLM context is cleared
- **Consistency** - depends on LLM memory
- **Scalability** - limited by LLM context window

## Known Issues

1. **Data consistency**: LLM may forget or hallucinate data between requests
2. **Complex queries**: Advanced query expressions may confuse LLM
3. **Large responses**: Very large item sets may exceed response size limits
4. **Request ID uniqueness**: Timestamp-based IDs may collide (very rare)
5. **Error codes**: Limited AWS error code vocabulary

## Example Responses

### GetItem Success

```json
{
  "actions": [
    {
      "type": "send_dynamo_response",
      "status_code": 200,
      "body": "{\"Item\":{\"id\":{\"S\":\"user-123\"},\"name\":{\"S\":\"Alice\"}}}"
    }
  ]
}
```

### PutItem Success

```json
{
  "actions": [
    {
      "type": "send_dynamo_response",
      "status_code": 200,
      "body": "{}"
    }
  ]
}
```

### Query Response

```json
{
  "actions": [
    {
      "type": "send_dynamo_response",
      "status_code": 200,
      "body": "{\"Items\":[{\"id\":{\"S\":\"user-123\"}}],\"Count\":1}"
    }
  ]
}
```

### Error Response

```json
{
  "actions": [
    {
      "type": "send_dynamo_response",
      "status_code": 400,
      "body": "{\"__type\":\"ResourceNotFoundException\",\"message\":\"Table not found\"}"
    }
  ]
}
```

## References

- [DynamoDB API Reference](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/)
- [DynamoDB JSON Format](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Programming.LowLevelAPI.html)
- [AWS SDK for Rust](https://github.com/awslabs/aws-sdk-rust) - for testing
- [DynamoDB Local](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/DynamoDBLocal.html)
