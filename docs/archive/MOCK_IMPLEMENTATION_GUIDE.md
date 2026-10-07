# Mock Implementation Guide for E2E Tests

This guide shows how to add mock LLM support to E2E tests for xmlrpc, xmpp, and zookeeper protocols.

## Pattern Overview

### 1. Server Test Mock Pattern

```rust
// BEFORE:
let server = helpers::start_netget_server(ServerConfig::new(prompt)).await?;

// AFTER:
let config = ServerConfig::new(prompt)
    .with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("<protocol>")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "<PROTOCOL_STACK>",
                    "instruction": "<instruction_summary>"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Protocol event
            .on_event("<event_name>")
            .and_event_data_contains("<key>", "<value>")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "<action_type>",
                    // ... action-specific fields
                }
            ]))
            .expect_calls(1)
            .and()
    });

let server = helpers::start_netget_server(config).await?;
```

### 2. Add Mock Verification

```rust
// BEFORE:
server.stop().await?;

// AFTER:
// Verify mock expectations were met
server.verify_mocks().await?;

server.stop().await?;
```

## XML-RPC Protocol

### Events and Actions

**Event**: `xmlrpc_method_call`
- Event data contains: `method_name`, `params`

**Actions**:
- `xmlrpc_success_response` - Returns success with value
  ```json
  {
    "type": "xmlrpc_success_response",
    "value_type": "int|string|boolean|array|struct",
    "value": <value>
  }
  ```

- `xmlrpc_fault_response` - Returns fault/error
  ```json
  {
    "type": "xmlrpc_fault_response",
    "fault_code": -32601,
    "fault_string": "Method not found"
  }
  ```

- `xmlrpc_list_methods_response` - Returns method list
  ```json
  {
    "type": "xmlrpc_list_methods_response",
    "methods": ["add", "subtract", "system.listMethods"]
  }
  ```

### Tests to Update

1. **test_xmlrpc_simple_method** - ✅ DONE
2. **test_xmlrpc_introspection_list_methods** - TODO
   - Mock event: xmlrpc_method_call with method_name="system.listMethods"
   - Mock action: xmlrpc_list_methods_response with methods=["add", "subtract", "multiply"]

3. **test_xmlrpc_fault_response** - TODO
   - Mock event: xmlrpc_method_call with method_name="nonExistentMethod"
   - Mock action: xmlrpc_fault_response with fault_code=-32601

4. **test_xmlrpc_string_parameter** - TODO
   - Mock event: xmlrpc_method_call with method_name="greet"
   - Mock action: xmlrpc_success_response with value_type="string", value="Hello, Alice!"

5. **test_xmlrpc_boolean_parameter** - TODO
   - Mock event: xmlrpc_method_call with method_name="toggle"
   - Mock action: xmlrpc_success_response with value_type="boolean", value=0

6. **test_xmlrpc_multiple_parameters** - TODO
   - Mock event: xmlrpc_method_call with method_name="concat"
   - Mock action: xmlrpc_success_response with value_type="string", value="Hello World"

7. **test_xmlrpc_non_post_request** - TODO
   - Mock only server startup (GET request won't trigger LLM)

## XMPP Protocol

### Events and Actions

**Event**: `xmpp_data_received`
- Event data contains: `xml_data`

**Actions**:
- `send_stream_header` - Send XMPP stream header
  ```json
  {
    "type": "send_stream_header",
    "from": "localhost",
    "stream_id": "stream-123"
  }
  ```

- `send_stream_features` - Send stream features
  ```json
  {
    "type": "send_stream_features",
    "mechanisms": ["PLAIN"]
  }
  ```

- `send_message` - Send XMPP message
  ```json
  {
    "type": "send_message",
    "from": "bot@localhost",
    "to": "alice@localhost",
    "message_type": "chat",
    "body": "Echo: Hello XMPP!"
  }
  ```

- `send_presence` - Send presence
  ```json
  {
    "type": "send_presence",
    "from": "server@localhost",
    "presence_type": "available",
    "status": "Server online"
  }
  ```

### Tests to Update

1. **test_xmpp_stream_header** - TODO
   - Mock event: xmpp_data_received (contains stream:stream)
   - Mock action: send_stream_header

2. **test_xmpp_message** - TODO
   - Mock event 1: xmpp_data_received (stream header)
   - Mock action 1: send_stream_header
   - Mock event 2: xmpp_data_received (message stanza)
   - Mock action 2: send_message

3. **test_xmpp_presence** - TODO
   - Mock event 1: xmpp_data_received (stream header)
   - Mock action 1: send_stream_header
   - Mock event 2: xmpp_data_received (presence stanza)
   - Mock action 2: send_presence

## ZooKeeper Protocol

### Events and Actions

**Event**: `zookeeper_request`
- Event data contains: `opcode`, `path`, `data`, `xid`

**Actions**:
- `zookeeper_response` - Generic ZooKeeper response
  ```json
  {
    "type": "zookeeper_response",
    "xid": 1,
    "zxid": 100,
    "error_code": 0,
    "data": "hello world",
    "stat": {
      "czxid": 100,
      "mzxid": 100,
      "ctime": 0,
      "mtime": 0,
      "version": 0,
      "cversion": 0,
      "aversion": 0,
      "dataLength": 11
    }
  }
  ```

### Tests to Update

ZooKeeper tests are currently placeholders (`#[ignore]`). Once real tests are implemented:

1. **test_zookeeper_server_basic** - TODO
   - Mock event: zookeeper_request with opcode for getData
   - Mock action: zookeeper_response with data

2. **test_zookeeper_get_data** - TODO
   - Mock event: zookeeper_request (getData operation)
   - Mock action: zookeeper_response

3. **test_zookeeper_create_node** - TODO
   - Mock event: zookeeper_request (create operation)
   - Mock action: zookeeper_response with success

4. **test_zookeeper_get_children** - TODO
   - Mock event: zookeeper_request (getChildren operation)
   - Mock action: zookeeper_response with children list

## Client Tests

**Note**: No client tests found for xmlrpc and zookeeper. Only XMPP has client tests.

### XMPP Client (`tests/client/xmpp/e2e_test.rs`)

Client tests would follow the same pattern but:
- Use `start_netget_client()` instead of `start_netget_server()`
- Mock `open_client` action for startup
- Mock client-specific events (e.g., `xmpp_connected`)
- Call `client.verify_mocks().await?` before cleanup

## Running Tests

```bash
# Mock mode (default, no Ollama required)
./test-e2e.sh xmlrpc
./test-e2e.sh xmpp
./test-e2e.sh zookeeper

# Real Ollama mode
./test-e2e.sh --use-ollama xmlrpc
./test-e2e.sh --use-ollama xmpp
./test-e2e.sh --use-ollama zookeeper

# With cargo directly
cargo test --features xmlrpc --test server -- server::xmlrpc::test
cargo test --features xmpp --test server -- server::xmpp::test
cargo test --features zookeeper --test server -- server::zookeeper::e2e_test
```

## Verification Checklist

For each test:
- [ ] Wrapped `ServerConfig::new()` with `.with_mock()`
- [ ] Mocked server startup with `open_server` action
- [ ] Mocked all protocol events that test triggers
- [ ] Set appropriate `expect_calls()` for each mock
- [ ] Added `server.verify_mocks().await?` before `server.stop()`
- [ ] Test compiles without errors
- [ ] Test passes in mock mode (default)
- [ ] Test passes with `--use-ollama` flag (optional validation)
