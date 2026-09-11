# OpenAPI Client Implementation

## Overview

The OpenAPI client implementation provides LLM-controlled HTTP/HTTPS requests driven by an OpenAPI specification. Instead of manually constructing requests, the LLM selects operations by ID and the client automatically constructs spec-compliant requests.

## Implementation Details

### Library Choices

- **openapi-rs** v1.0 - OpenAPI 3.x spec parser
  - Same library used by OpenAPI server
  - Parses YAML/JSON specifications
  - Provides typed access to paths, operations, parameters
- **reqwest** - Modern async HTTP client for Rust, built **once** for the process on
  `spawn_blocking` (`OpenApiClient::http_client`). Building it loads the platform root store,
  which on macOS is a synchronous keychain read; on the async runtime that parks a tokio
  worker. It used to be rebuilt per operation *and* built once at connect and dropped.
  - Same library used by HTTP client
  - Supports HTTP/1.1, HTTP/2, HTTPS (TLS via rustls)
  - Automatic protocol negotiation via ALPN

### Architecture

```
┌──────────────────────────────────────────┐
│  OpenApiClient::connect_with_llm_actions │
│  - Parse OpenAPI spec (YAML/JSON)       │
│  - Extract operation list               │
│  - Store spec + base URL                │
│  - Mark as Connected                    │
└──────────────────────────────────────────┘
         │
         ├─► execute_operation() - Called per LLM action
         │   - Find operation in spec by ID
         │   - Substitute path parameters
         │   - Build HTTP request from spec
         │   - Execute via reqwest
         │   - Call LLM with response
         │   - Update memory
         │
         └─► Background Monitor Task
             - Checks if client still exists
             - Exits if client removed
```

### Connection Model

Unlike TCP (persistent connection), OpenAPI client is **request/response** based:

- "Connection" = initialization with spec parsing
- Each request is independent
- LLM triggers requests via `execute_operation` action
- Responses trigger LLM calls for interpretation

### Spec-Driven Request Construction

**1. Spec Loading:**
- Inline spec via `spec` parameter (YAML or JSON string)
- File spec via `spec_file` parameter
- Parsed using `openapi-rs` at startup

**2. Operation Discovery:**
- Iterate over all paths in spec
- Extract operations (GET, POST, PUT, DELETE, etc.)
- Send operation list to LLM in `openapi_client_connected` event

**3. Operation Execution:**
```rust
// LLM action
{
  "type": "execute_operation",
  "operation_id": "getUser",
  "path_params": {"id": "123"},
  "query_params": {"fields": "name,email"},
  "headers": {"Authorization": "Bearer token"},
  "body": null
}

// Client processing
1. Find operation in spec by operation_id
2. Get path template: "/users/{id}"
3. Substitute path params: "/users/123"
4. Build URL: base_url + "/users/123" + "?fields=name,email"
5. Set method from spec: GET
6. Add headers (spec defaults + user overrides)
7. Execute HTTP request via reqwest
```

**4. Path Parameter Substitution:**
```rust
fn substitute_path_params(
    template: &str,         // "/users/{id}/posts/{post_id}"
    params: &HashMap<...>,  // {"id": "123", "post_id": "456"}
) -> Result<String> {
    // Result: "/users/123/posts/456"
    let mut path = template.to_string();
    for (key, value) in params {
        let pattern = format!("{{{}}}", key);
        path = path.replace(&pattern, value);
    }
    // Validate: check for unsubstituted parameters
    if path.contains('{') {
        return Err(...); // Missing required parameter
    }
    Ok(path)
}
```

### LLM Control

**Async Actions** (user-triggered):

- `execute_operation` - Execute operation by ID
  - Parameters: operation_id, path_params, query_params, headers, body
  - Returns Custom result with request data
- `list_operations` - List all operations (no-op, already sent in connected event)
- `get_operation_details` - Get operation details (future enhancement)
- `disconnect` - Stop OpenAPI client

**Sync Actions** (in response to operation responses):

- `execute_operation` - Make follow-up request based on response

**Events:**

- `openapi_client_connected` - Fired when client initialized with spec
  - Data includes: base_url, spec_title, spec_version, operation_count, operations (array)
- `openapi_operation_response` - Fired when response received
  - Data includes: operation_id, method, path, status_code, status_text, headers, body

### Structured Actions (CRITICAL)

OpenAPI client uses **structured data**, NOT raw bytes:

```json
// Request action
{
  "type": "execute_operation",
  "operation_id": "listUsers",
  "path_params": {},
  "query_params": {"page": "1", "limit": "10"},
  "headers": {"Accept": "application/json"},
  "body": null
}

// Response event
{
  "event_type": "openapi_operation_response",
  "data": {
    "operation_id": "listUsers",
    "method": "GET",
    "path": "/users",
    "status_code": 200,
    "status_text": "OK",
    "headers": {
      "Content-Type": "application/json"
    },
    "body": "[{\"id\": 1, \"name\": \"Alice\"}]"
  }
}
```

LLMs can select operations and provide parameters without knowing URL structure.

### Request Flow

1. **LLM Action**: `execute_operation` with operation_id and parameters
2. **Action Execution**: Returns `ClientActionResult::Custom` with operation data
3. **Request Construction**: `OpenApiClient::execute_operation()` called
4. **Spec Lookup**: Find operation in spec, get method and path template
5. **Path Building**: Substitute parameters, add query string
6. **HTTP Execution**: Build and send reqwest request
7. **Response Handling**:
   - Parse status, headers, body
   - Create `openapi_operation_response` event
   - Call LLM for interpretation
8. **LLM Response**: May trigger follow-up operations

### Startup Parameters

- `spec` (optional) - OpenAPI spec in YAML or JSON (inline)
- `spec_file` (optional) - Path to OpenAPI spec file
- `base_url` (optional) - Override base URL. The default is `remote_addr`, **not** the spec's
  `servers[0]`; see "Base URL Handling" for why the order matters.

**Note:** Either `spec` or `spec_file` is required.

### Dual Logging

```rust
info!("OpenAPI client {} executing operation '{}': {} {}",
      client_id, operation_id, method, url);  // → netget.log
status_tx.send("[CLIENT] OpenAPI operation executed");  // → TUI
```

### Error Handling

- **Spec Parse Failed**: Error, client not created
- **Operation Not Found**: Error, request not sent
- **Missing Path Params**: Error, request not sent
- **Request Failed**: Log error, call LLM with error event (future)
- **Timeout**: 30s, set on the shared client
- **Response size**: bounded at 8 MiB. `response.text()` buffers the whole body with no limit
  and the body is remote-controlled — it goes into an `openapi_operation_response` event and
  from there into an LLM prompt. It also used `unwrap_or_default()`, which turned a read
  failure into an empty body: downstream that reads as "the server answered with nothing"
  rather than "we could not read the answer"
- **LLM Error**: Log, continue accepting actions

## Features

### Supported Features

- ✅ OpenAPI 3.x spec parsing (YAML and JSON)
- ✅ Operation lookup by operation_id
- ✅ Path parameter substitution
- ✅ Query parameter merging
- ✅ Header merging (spec + user overrides)
- ✅ JSON request bodies
- ✅ HTTPS (TLS via rustls)
- ✅ HTTP/1.1 and HTTP/2 (automatic negotiation via ALPN)
- ✅ Timeouts (30s default)

### Base URL Handling

Priority order:
1. `base_url` parameter (an explicit override, so it wins over everything)
2. `remote_addr` — the address the operator or model actually named. Scheme-qualified is used
   as given; a bare `host:port` gets `http://`, because reqwest needs an absolute URL and a
   relative one fails every request in a way that presents as the server being unreachable
3. The first entry of the spec's `servers` array, only when nothing was named — and it logs
   that it is doing so
4. Otherwise the client refuses to connect

**Order 2 and 3 used to be the other way round, and that was a real defect.** The spec's
declared server won over `remote_addr`, so an operator who said "connect to 127.0.0.1:9000"
and handed over a spec whose `servers:` block names `https://api.production.example.com` got
the production host — silently, with no log line saying the named address had been discarded.
A spec is data supplied with the request, frequently by the model, so it must not be able to
retarget the client. This is the DynamoDB shape from the root `CLAUDE.md` reached by a
different route: a client that loses its target and arrives at a real service.

The tell was sitting in the test suite the whole time. `tests/client/openapi/e2e_test.rs`
rewrites `{port}` *inside the spec* before every run, which is the workaround you write when
`remote_addr` is not what decides. `target_precedence_test.rs` now pins all three levels.

Example:
```yaml
openapi: 3.1.0
servers:
  - url: https://api.example.com/v1
    description: Production
  - url: https://staging.api.example.com/v1
    description: Staging
```

Client uses first server: `https://api.example.com/v1`

## Limitations

- **No Response Validation** - Does not validate responses against spec schemas
- **No Request Validation** - Does not validate request bodies against schemas (trusts LLM)
- **No Authentication Flows** - Does not automatically handle OAuth2/OpenID Connect flows defined in spec
- **Single Base URL** - Uses one base URL (cannot switch between multiple servers)
- **No Async Operations** - Does not support WebSocket/SSE operations in spec
- **No File Uploads** - Body is JSON only (no multipart/form-data)
- **No Cookie Jar** - Each request independent (no session management)
- **No Schema Defaults** - Does not apply default values from schemas

## Usage Examples

### Example 1: Simple API Testing

**User**: "Connect to api.example.com and test the users endpoint"

**Startup Params**:
```json
{
  "spec_file": "/path/to/openapi.yaml",
  "base_url": "https://api.example.com"
}
```

**LLM Flow**:
1. Connected event shows operations: `listUsers`, `createUser`, `getUser`
2. `execute_operation("listUsers")`
3. Response: 200 OK, body: `[{"id": 1, "name": "Alice"}]`
4. `execute_operation("getUser", path_params={"id": "1"})`
5. Response: 200 OK, body: `{"id": 1, "name": "Alice", "email": "alice@example.com"}`

### Example 2: Inline Spec

**User**: "Test this API" (provides spec inline)

**Startup Params**:
```json
{
  "spec": "openapi: 3.1.0\ninfo:\n  title: TODO API\n  version: 1.0.0\nservers:\n  - url: https://jsonplaceholder.typicode.com\npaths:\n  /todos:\n    get:\n      operationId: listTodos\n      responses:\n        '200':\n          description: List todos",
  "base_url": null
}
```

**LLM Flow**:
1. Spec parsed. The base URL is **`remote_addr`**, not the spec's
   `https://jsonplaceholder.typicode.com` — the spec's server is used only when no address
   was named, and the client logs when it ignores one. Pass `base_url` to force the spec's
   host.
2. Connected event shows: `listTodos`
3. `execute_operation("listTodos")`
4. Response: 200 OK, body: `[{...}, {...}]`

### Example 3: Path Parameters

**User**: "Get user 123's profile"

**LLM Action**:
```json
{
  "type": "execute_operation",
  "operation_id": "getUser",
  "path_params": {"id": "123"}
}
```

**Client Processing**:
- Path template: `/users/{id}`
- Substituted: `/users/123`
- URL: `https://api.example.com/users/123`
- Method: GET (from spec)

## Testing Strategy

See `tests/client/openapi/CLAUDE.md` for E2E testing approach with mocks.

## Benefits vs HTTP Client

1. **Spec-Driven**: LLM doesn't need to memorize paths/methods
2. **Type Safety**: Parameters defined by spec
3. **Self-Documenting**: Operation list sent to LLM automatically
4. **Path Parameters**: Automatic substitution
5. **Consistency**: All requests follow spec structure
6. **No URL Construction**: LLM provides operation ID, not full path
7. **Parameter Discovery**: LLM sees available parameters from spec

## Future Enhancements

- **Response Schema Validation**: Use `jsonschema` crate to validate responses
- **Request Schema Validation**: Validate request bodies before sending
- **Authentication Integration**: Auto-handle security schemes from spec (Bearer, OAuth2, etc.)
- **Multi-Server Support**: Allow LLM to choose server from spec's servers array
- **WebSocket/SSE Support**: Handle async operations defined in OpenAPI spec
- **Default Values**: Apply default values from parameter schemas
- **Example Values**: Use example values from spec for testing
- **Error Responses**: Send error events to LLM when requests fail

## Injected commands (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running OpenAPI client and gets back a truthful `ClientSendOutcome`. See `src/client/command_support.rs` and `tests/client/openapi/command_channel_test.rs`.

- `command_support::register_command_channel` runs **before** anything that can block for a
  human — specifically before the `openapi_client_connected` `call_llm_for_client`, so `[ send ]` works even while an event is parked on a manual routing
  rule. This is the whole point of the feature: registering late means the rail reads
  "no command channel" for the length of the park.
- The command loop **replaced the old 5-second `get_client().is_none()` idle poll**. When the
  client is removed its handle is dropped, the channel closes, `recv()` returns `None` and the
  loop exits at once — the poll was strictly slower at the same job. The loop is registered
  with `register_client_task`, and it drops the handle on every exit path.
- One `apply_action` is the only place actions become traffic, shared by the command loop and `execute_llm_actions` (the `openapi_client_connected` handler), which spawns it so `connect()` can return. There is
  no second wire path for injected actions to drift from.

### Outcome semantics — what `[ send ]` reports, and why

| Outcome | When |
|---|---|
| `Sent { bytes_sent }` | **Never.** `reqwest` does not report how many bytes reached the wire, and inventing a number would be a lie |
| `Executed { detail }` | The operation ran to completion; `detail` names it (`operation listUsers completed ...`). Spec resolution and path-parameter substitution happen on this path exactly as on the LLM path, because it *is* the same path |
| `Rejected { error }` | `execute_action` refused the JSON (unknown verb, missing field) |
| `Disconnected` | `disconnect`; the loop ends, the handle is dropped and the client goes to `Disconnected` |
| `Err(...)` | The request itself failed (transport error, non-2xx). The caller sees the error rather than a false success |

**The known cost of awaiting.** `apply_action` awaits the whole operation, *including the
response event it raises*. That is what makes the outcome truthful, but it also means the
command loop is busy until that event has been handled — with a `*` -> manual rule, until the
human answers it or the intercept times out (default 300s). Injected sends queue behind it on
the bounded channel, which surfaces as "client busy" backpressure. `send_to_client`'s own
timeout protects the caller either way.

**Not wired:** `execute_operation` still discards the actions the LLM returns for `openapi_operation_response` (`actions: _`, with a comment blaming async recursion). The command channel does not change that, but it does give the operator a way to drive follow-up operations by hand. An `operation_id` that is not in the spec produces `Err`, not a false `Executed` — asserted in the test.
