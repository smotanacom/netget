# MCP Client E2E Test Strategy

## Overview

Black-box E2E tests for MCP (Model Context Protocol) client using real NetGet binary with Ollama LLM integration. Tests
verify client connection, initialization, and MCP operations (tools, resources, prompts).

## Test Approach

### Black-Box Testing

- Spawn real `netget` binary with MCP server and client instances
- Use actual Ollama LLM (not mocked)
- Verify behavior via output inspection
- Tests are protocol-agnostic (don't inspect internal state)

### Test Environment

- **LLM Required**: Yes (all tests call Ollama)
- **Network**: Localhost only (127.0.0.1)
- **Ports**: Dynamic allocation via `{AVAILABLE_PORT}` placeholder
- **Concurrency**: Tests use a per-test in-process mock LLM

## LLM Call Budget

**Total: < 10 LLM calls across all tests**

### Test 1: `test_mcp_client_initialize` - 2 LLM calls

1. Server startup (LLM declares MCP capabilities)
2. Client connection (LLM receives connected event)

**Rationale**: Minimal initialization test verifies three-phase handshake works.

### Test 2: `test_mcp_client_call_tool` - 3 LLM calls

1. Server startup (LLM declares tools)
2. Client connection (LLM lists tools)
3. Client tool call (LLM calls calculate tool)

**Rationale**: Tests complete tool workflow: list → call → response.

### Test 3: `test_mcp_client_read_resource` - 3 LLM calls

1. Server startup (LLM declares resources)
2. Client connection (LLM lists resources)
3. Client resource read (LLM reads specific resource)

**Rationale**: Tests resource workflow: list → read → response.

**Total Budget: 2 + 3 + 3 = 8 LLM calls** ✅

## Test Coverage

### Initialization (Test 1)

- ✅ Three-phase handshake (initialize → response → initialized)
- ✅ Client receives server capabilities
- ✅ Client status becomes Connected

### Tool Operations (Test 2)

- ✅ List tools (JSON-RPC `tools/list`)
- ✅ Call tool (JSON-RPC `tools/call`)
- ✅ Receive tool result

### Resource Operations (Test 3)

- ✅ List resources (JSON-RPC `resources/list`)
- ✅ Read resource (JSON-RPC `resources/read`)
- ✅ Receive resource content

### Not Covered (Acceptable)

- ❌ Prompts (similar to tools/resources, low priority)
- ❌ Resource subscriptions (server push, complex)
- ❌ Error handling (requires more LLM calls)
- ❌ Concurrent operations (single-threaded test)

## Expected Runtime

**Per Test:**

- Server startup: ~500ms
- Client connection: ~1-2s (includes LLM call and initialization)
- Operations: ~1-2s per LLM call
- Cleanup: ~100ms

**Total per test: 3-5 seconds**
**All tests: 10-15 seconds**

With Ollama lock and serial execution, total suite runtime: **~20 seconds**

## Known Issues & Flakiness

### Potential Issues

1. **LLM Interpretation**: LLM may not immediately recognize MCP protocol from prompt
    - Mitigation: Clear prompt with "via MCP" instruction

2. **JSON-RPC Parsing**: LLM may struggle with JSON-RPC response formatting
    - Mitigation: Server provides clear action examples

3. **Timing**: Client may send requests before server is ready
    - Mitigation: 500ms sleep after server startup

4. **Output Inspection**: Tests rely on string matching in output
    - Mitigation: Flexible assertions (OR conditions)

### Flaky Test Indicators

- Tests should pass >95% of the time
- If tests fail, check Ollama model availability
- Check for port conflicts (unlikely with dynamic allocation)

## Test Execution

### Run All MCP Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features mcp --test client::mcp::e2e_test
```

### Run Specific Test

```bash
./cargo-isolated.sh test --no-default-features --features mcp --test client::mcp::e2e_test test_mcp_client_initialize
```

### Debug Output

Tests capture NetGet stdout/stderr. On failure, output is printed for debugging.

## Success Criteria

**All tests must:**

1. Complete within expected runtime (< 10s per test)
2. Use ≤ budgeted LLM calls
3. Verify client protocol is "MCP"
4. Show relevant output (connection, tools, resources)
5. Clean up gracefully (no zombie processes)

## Comparison with Other Client Tests

| Aspect         | MCP Client                     | HTTP Client           | TCP Client           |
|----------------|--------------------------------|-----------------------|----------------------|
| **LLM Calls**  | 8 total                        | 4 total               | 6 total              |
| **Complexity** | Medium (JSON-RPC)              | Low (HTTP)            | Low (Raw TCP)        |
| **Operations** | 3 types (init, tool, resource) | 2 types (GET, custom) | 2 types (send, echo) |
| **Protocol**   | HTTP + JSON-RPC                | HTTP                  | Raw TCP              |
| **Runtime**    | ~20s                           | ~10s                  | ~15s                 |

## Future Enhancements

**If budget allows (> 10 calls):**

- Test prompts (list + get)
- Test error handling (invalid tool/resource)
- Test multiple sequential operations
- Test resource subscriptions

**If architecture changes:**

- Add WebSocket transport tests
- Add SSE (Server-Sent Events) tests
- Add progress notification tests
