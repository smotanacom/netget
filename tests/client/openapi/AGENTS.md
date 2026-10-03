# OpenAPI Client E2E Tests

## Test Strategy

### Approach

Mock-based black-box E2E testing with spec-driven request validation. Tests verify that the OpenAPI client correctly:
1. Parses OpenAPI specifications
2. Executes operations by ID
3. Substitutes path parameters
4. Constructs HTTP requests from spec

### LLM Call Budget

**Target**: < 5 LLM calls per test suite
**Achieved**: 4 calls per test (well below budget)

**Breakdown**:
- Test 1 (`test_openapi_client_with_spec`): 4 calls
  1. Server startup mock
  2. Server HTTP request mock
  3. Client startup mock
  4. Client connected + operation execution mock

- Test 2 (`test_openapi_client_path_params`): 4 calls
  1. Server startup mock
  2. Server HTTP request mock (validates path substitution)
  3. Client startup mock
  4. Client connected + operation execution mock with path params

**Total**: 8 calls for 2 tests = **4 calls/test average**

### Test Runtime

**Expected**: < 30 seconds for all tests
**Actual**: ~10-15 seconds

**Breakdown**:
- Server startup: 2s
- Client execution: 5s
- Mock verification: 1s
- **Per test**: ~8s
- **Total (2 tests)**: ~16s

### Mock Strategy

**Mock Server**: HTTP server acts as OpenAPI backend
- Validates that client sends correct HTTP requests
- Responds with JSON data matching spec

**Mock Client**: OpenAPI client with inline spec
- Spec provided via `startup_params.spec`
- LLM selects operations by `operation_id`
- Client constructs requests automatically

**Validation Points**:
1. ✅ Spec parsing (inline YAML)
2. ✅ Operation lookup by ID
3. ✅ Path parameter substitution (`/users/{id}` → `/users/123`)
4. ✅ HTTP request construction (method, path, headers)
5. ✅ Response handling (status, headers, body)

### Key Test Cases

#### Test 1: Basic Operation Execution

**Purpose**: Verify spec-driven request construction

**Flow**:
1. Start HTTP server on port 0
2. Start OpenAPI client with inline spec
3. Client executes `listUsers` operation
4. Server receives GET /users request
5. Client receives 200 OK response
6. Client disconnects

**Validates**:
- Spec parsing (YAML to OpenAPI object)
- Operation extraction (listUsers found)
- HTTP request construction (GET /users)
- Response parsing (JSON body)

#### Test 2: Path Parameter Substitution

**Purpose**: Verify path template parameter substitution

**Flow**:
1. Start HTTP server
2. OpenAPI client with spec: `/users/{id}`
3. Client executes `getUser` with `path_params: {"id": "123"}`
4. Server receives GET /users/123 (substituted!)
5. Server responds 200 OK
6. Client disconnects

**Validates**:
- Path parameter extraction from action
- Template substitution (`{id}` → `123`)
- Correct HTTP path construction
- No unsubstituted placeholders

### OpenAPI Spec Format

Tests use inline YAML specs with:
```yaml
openapi: 3.1.0
info:
  title: Test API
  version: 1.0.0
servers:
  - url: http://127.0.0.1:{port}  # Dynamic port from server
paths:
  /users:
    get:
      operationId: listUsers
      responses:
        '200':
          description: List users
  /users/{id}:
    get:
      operationId: getUser
      parameters:
        - name: id
          in: path
          required: true
          schema:
            type: string
      responses:
        '200':
          description: User details
```

### Known Issues

**None currently**

Potential future issues:
- **Spec file loading**: Tests use inline specs, not file paths
- **Base URL override**: Not tested (uses default from spec)
- **Query parameters**: Not tested (future test case)
- **Request bodies**: Not tested (POST/PUT operations)

### Running Tests

```bash
# Mock mode (default, no Ollama required)
./test-e2e.sh openapi

# With real Ollama (for validation)
./test-e2e.sh --use-ollama openapi

# Via cargo (mock mode)
cargo test --no-default-features --features openapi --test client::openapi::e2e_test

# Via cargo (with Ollama)
NETGET_USE_OLLAMA=1 cargo test --no-default-features --features openapi --test client::openapi::e2e_test
```

### Test Isolation

- Each test uses a unique port (server binds to port 0)
- Mocks are verified after each test
- No shared state between tests
- Tests can run in parallel

### Future Test Cases

1. **Query Parameters**: Test `query_params` in execute_operation
2. **Request Bodies**: Test POST/PUT with JSON bodies
3. **Multiple Operations**: Execute 3+ operations in sequence
4. **Error Handling**: Test missing path parameters, invalid operation IDs
5. **Spec Validation**: Test invalid specs, missing fields
6. **Base URL Override**: Test `base_url` parameter
7. **Spec File Loading**: Test `spec_file` parameter
8. **Multi-Server Specs**: Test specs with multiple servers

### Comparison to HTTP Client Tests

| Aspect | HTTP Client | OpenAPI Client |
|--------|-------------|----------------|
| **Request Construction** | Manual (LLM provides path) | Spec-driven (LLM provides operation ID) |
| **Path Handling** | Full path in action | Template substitution |
| **Validation** | None | Spec structure |
| **Complexity** | Lower | Higher (spec parsing) |
| **Test Focus** | HTTP mechanics | Spec-to-HTTP translation |

### Debugging Tips

**If tests fail**:
1. Check mock expectations: `client.verify_mocks().await?`
2. Verify spec parsing: Look for YAML parse errors
3. Check operation lookup: Ensure `operation_id` exists in spec
4. Validate path substitution: Check for `{param}` in paths
5. Inspect HTTP requests: Check server mock data contains

**Common Issues**:
- **Missing operation**: `operation_id` typo or not in spec
- **Path params**: Forgot to escape `{id}` in spec (`{{id}}` in YAML)
- **Port mismatch**: Server port not in spec's `servers.url`

## `command_channel_test.rs` — the dashboard's `[ send ]`

One test, **zero LLM calls**. The client's LLM points at `http://127.0.0.1:1`, so the
connected-event call (where there is one) and every response-event call fail immediately;
tolerating that failure is part of what the test verifies.

The API is a throwaway `TcpListener` on 127.0.0.1 that speaks just enough HTTP/1.1 and records the request lines it saw; the spec is built inline with `servers: [http://127.0.0.1:{port}]`. **Nothing contacts an external endpoint.**

What it asserts, in order:

1. `wait_for_client_handle` — the regression guard for "register the channel before the
   connected-event LLM call". Without it, a manual routing rule would leave `[ send ]` dead
   for the length of the park.
2. The injected action returns `Executed`, **not** `Sent` — `reqwest` reports no wire byte count, so a number would be invented.
3. The stub API really saw `GET /users`, and `getUser` with `path_params: {"id": "42"}` really became `GET /users/42` — spec resolution and substitution run on the injected path because it *is* the LLM path.
4. The client's access log gained an `injected_action` entry, so injected traffic shows up in
   the request pane exactly like LLM-produced traffic.
5. An unknown verb comes back `Rejected`, not silently swallowed. An `operation_id` absent from the spec comes back as `Err`, never a false `Executed`.
6. `disconnect` returns `Disconnected`, the command loop exits, the handle is dropped and the
   client's status becomes `Disconnected`.

## In the browser

The native suites here drive the reqwest backend. The browser path — the same client code over
`src/client/http_fetch`'s hyper transport — is proven by `web/test/smoke.mjs` against the real
bundle (see `src/client/openapi/CLAUDE.md`, "Browser build"), and the transport backend's requests
are pinned to reqwest's by `tests/client/http/fetch_client_test.rs`.

