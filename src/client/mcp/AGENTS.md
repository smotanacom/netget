# MCP (Model Context Protocol) Client Implementation

## Overview

MCP client implementing the Model Context Protocol specification for connecting to MCP servers and accessing their
capabilities (resources, tools, prompts). Built on JSON-RPC 2.0 over HTTP with three-phase initialization.

## Protocol Version

- **MCP**: 2024-11-05 specification
- **Transport**: HTTP POST with JSON-RPC 2.0 messages
- **Framework**: https://modelcontextprotocol.io/

## Library Choices

### Core Dependencies

- **reqwest** - HTTP client library
    - Chosen for: Async HTTP requests, JSON support, wide adoption
    - Used for: HTTP POST requests to MCP server
- **serde_json** - JSON handling
- **tokio** - Async runtime

### Custom JSON-RPC Implementation

**Why Not Use a JSON-RPC Library?**

- MCP has specific JSON-RPC patterns (initialize flow, notifications)
- Custom implementation provides full control over MCP semantics
- Simple enough to implement directly (borrowed from server implementation)

## Architecture Decisions

### Three-Phase Initialization

**MCP Handshake**:

1. **Client → initialize request** (with clientInfo, capabilities)

`clientInfo` is the `client_name` / `client_version` startup parameters, falling back to
`netget-mcp-client` / `0.1.0` (`DEFAULT_CLIENT_NAME` / `DEFAULT_CLIENT_VERSION`). This is the
only place the pair is used, and it is the only thing the server learns about who connected —
a server that logs or gates on `clientInfo.name` sees exactly what the operator set.

2. **Server → initialize response** (with serverInfo, capabilities)
3. **Client → `notifications/initialized`** (confirms connection)

Only after phase 3 can the client make resource/tool/prompt requests.

**The method name is `notifications/initialized`, not `initialized`.** MCP namespaces every
notification under `notifications/`, and this client sent the bare name — so phase 3 was an
unrecognised notification to every server it ever spoke to, **including NetGet's own MCP
server**, whose router matches `notifications/initialized` and drops anything else into a
`debug!("Unknown MCP notification")`. Nothing failed visibly because a notification has no
reply: the client logged that it had sent one, got its 204, and declared the handshake
complete. It survived because all three MCP client e2e tests were `#[ignore]`d.
`tests/client/mcp/e2e_test.rs::test_mcp_client_initialize` now asserts on the **server's** log
line, which is the only place the difference is visible.

### Connection Model

- **HTTP-based**: No persistent connection (unlike TCP/Redis clients)
- **Request-Response**: Each MCP operation is a separate HTTP POST
- **Stateless HTTP**: Server tracks session via JSON-RPC, not TCP connection
- **Request ID Tracking**: Client maintains request ID counter for matching responses

### LLM Control Points

**Complete MCP Operations Control** - LLM decides what to do after connection:

1. **After Initialize**: LLM receives connected event with server capabilities
2. **list_resources**: LLM requests available resources
3. **read_resource**: LLM reads specific resource by URI
4. **list_tools**: LLM requests available tools
5. **call_tool**: LLM executes tool with arguments
6. **list_prompts**: LLM requests available prompts
7. **get_prompt**: LLM retrieves prompt template

**Event-Driven Flow**:

1. Client connects and initializes → `mcp_client_connected` event
2. LLM receives event with server capabilities
3. LLM decides which actions to take (list tools, call tool, etc.)
4. Client executes action → sends JSON-RPC request
5. Client receives response → `mcp_response_received` event
6. LLM processes response → decides next action
7. Repeat until LLM disconnects

### State Management

**Client State (in AppState protocol_data)**:

```rust
{
    "base_url": "http://localhost:8000",
    "request_id": 5,
    "initialized": true,
    "server_info": {
        "name": "example-server",
        "version": "1.0.0"
    },
    "capabilities": {
        "resources": {"subscribe": true},
        "tools": {},
        "prompts": {}
    }
}
```

**Request ID Management**:

- Starts at 1
- Incremented for each JSON-RPC request
- Used to match responses to requests

### LLM Integration Flow

**Initial Connection**:

1. `connect_with_llm_actions()` called
2. Send initialize request (Phase 1)
3. Receive initialize response (Phase 2)
4. Send initialized notification (Phase 3)
5. Create `mcp_client_connected` event
6. Call LLM with event
7. Execute LLM actions (recursive)

**Action Execution Cycle**:

1. LLM returns actions (e.g., `call_tool`)
2. `execute_action()` parses action JSON
3. Returns `ClientActionResult::Custom`
4. `execute_mcp_action()` sends JSON-RPC request
5. Receive JSON-RPC response
6. Create `mcp_response_received` event
7. Call LLM with event (recursive)

**Recursive Pattern**:
The LLM can chain multiple operations naturally:

- Initialize → List Tools → Call Tool → Read Resource → Disconnect

## Limitations

### Not Implemented

- **Streaming**: No streaming responses (single HTTP POST per request)
- **Server Push**: No server-initiated events (would require WebSocket/SSE)
- **Subscriptions**: Resource subscriptions tracked but no change notifications
- **Progress**: Progress tracking not implemented
- **Cancellation**: Request cancellation not implemented
- **Sampling**: LLM sampling API not exposed

### Session Management

- **No Server Session**: HTTP is stateless, server may track session separately
- **Client State Only**: Client tracks initialization state, server capabilities
- **No Expiration**: Client state never times out
- **No Reconnection**: If disconnected, must create new client

### Error Handling

- **JSON-RPC Errors**: Parsed and returned as errors
- **HTTP Errors**: Non-2xx status codes returned as errors
- **Network Errors**: Timeouts (30s) and connection failures
- **No panics on missing client state.** `execute_mcp_action` read `base_url` and `request_id`
  with `.expect(...)` *inside* the `with_client_mut` closure, so a client whose protocol fields
  had not been set — or had been reset — panicked while the `AppState` write guard was held.
  The panic landed inside the spawned command loop or notify task, which swallows it, so the
  symptom was a client that silently stopped answering: no error, no log line, and `[ send ]`
  timing out on a request that was never made. Both are `Option` + `context(...)` now.
- **`prompts/get` omits `arguments` when absent** rather than sending `"arguments": null`. MCP
  declares it optional, and a server validating it against its declared object type rejects an
  explicit null; `Some(json!({… "arguments": None}))` serialised to exactly that.

## Example Prompts and LLM Flow

### Startup

```
Connect to http://localhost:8000 via MCP.

List available tools, then call the 'calculate' tool with expression "2+2".
```

### Expected LLM Flow

**Phase 1: Initialize** (automatic)

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "2024-11-05",
    "capabilities": {"roots": {"listChanged": false}},
    "clientInfo": {"name": "netget-mcp-client", "version": "0.1.0"}
  }
}
```

**Phase 2: Server Response**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2024-11-05",
    "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
    "serverInfo": {"name": "example-server", "version": "1.0.0"}
  }
}
```

**Phase 3: Initialized Notification** (automatic)

```json
{
  "jsonrpc": "2.0",
  "method": "notifications/initialized",
  "params": {}
}
```

**LLM Receives Connected Event**:

```json
{
  "event_type": "mcp_client_connected",
  "data": {
    "server_name": "example-server",
    "server_version": "1.0.0",
    "capabilities": {"tools": {}, "resources": {}, "prompts": {}}
  }
}
```

**LLM Action 1: List Tools**:

```json
{
  "actions": [
    {
      "type": "list_tools"
    }
  ]
}
```

**Client Sends**:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/list"
}
```

**Server Responds**:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "tools": [
      {
        "name": "calculate",
        "description": "Evaluate math expressions",
        "inputSchema": {
          "type": "object",
          "properties": {
            "expression": {"type": "string"}
          }
        }
      }
    ]
  }
}
```

**LLM Receives Response Event**:

```json
{
  "event_type": "mcp_response_received",
  "data": {
    "method": "mcp_list_tools",
    "result": {
      "tools": [...]
    }
  }
}
```

**LLM Action 2: Call Tool**:

```json
{
  "actions": [
    {
      "type": "call_tool",
      "name": "calculate",
      "arguments": {
        "expression": "2+2"
      }
    }
  ]
}
```

**Client Sends**:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "calculate",
    "arguments": {"expression": "2+2"}
  }
}
```

**Server Responds**:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "content": [
      {"type": "text", "text": "4"}
    ]
  }
}
```

**LLM Receives Response Event** and decides to disconnect.

## Key Design Principles

1. **MCP Compliance** - Implements MCP 2024-11-05 client spec
2. **LLM-Driven** - All operations controlled by LLM after initialization
3. **Event-Based** - Connected and response events trigger LLM decisions
4. **HTTP-Based** - Stateless HTTP POST per request (no persistent connection)
5. **JSON-RPC Foundation** - Built on JSON-RPC 2.0 substrate
6. **Recursive Actions** - LLM actions can chain naturally via events

## Comparison with Other Clients

| Aspect             | MCP Client                  | HTTP Client                 | TCP Client              |
|--------------------|-----------------------------|-----------------------------|-------------------------|
| **Connection**     | HTTP POST per request       | HTTP request per action     | Persistent TCP socket   |
| **State**          | Client-side only            | Stateless                   | Connection-based        |
| **LLM Trigger**    | Connected + Response events | Connected + Response events | Connected + Data events |
| **Initialization** | 3-phase handshake           | Immediate                   | Connect only            |
| **Protocol**       | JSON-RPC 2.0                | HTTP                        | Raw bytes               |

## References

- [Model Context Protocol Specification](https://modelcontextprotocol.io/)
- [MCP Server Implementation](../../server/mcp/CLAUDE.md)
- [reqwest Documentation](https://docs.rs/reqwest/)
- [JSON-RPC 2.0 Specification](https://www.jsonrpc.org/specification)

## Injected actions (the dashboard's `[ send ]`)

The client registers a command channel and spawns `McpClient::command_loop` **before** the
`mcp_connected` LLM call, because a `*` -> manual rule can park that call for minutes and the
operator must still be able to reach the client. The command task also replaces the old 5s
"has the client been removed yet" poll inside the LLM task.

`McpClient::apply_action` is the single place a JSON-RPC call is issued and its
`mcp_response_received` event fired; `execute_llm_actions` and the command loop both go
through it, so an injected `list_tools` produces the same `tools/list` POST (and the same
follow-up action chain) the LLM path would. It is boxed because it recurses back into
`execute_llm_actions` for the follow-ups.

`ClientSendOutcome` semantics:

| Outcome | When |
|---|---|
| `Executed { detail }` | The JSON-RPC call ran, e.g. `mcp_list_tools completed`. |
| `Rejected { error }` | `execute_action` refused the action (unknown verb, missing `uri`/`name`). |
| `Disconnected` | `{"type":"disconnect"}`; status goes to Disconnected and the handle is dropped. |
| `Err(...)` | The POST failed, the server returned a JSON-RPC error, or the body did not parse. |

**`Sent { bytes_sent }` is never reported**: reqwest owns the socket, so a byte count would
be invented. The call is awaited before the outcome is returned.
