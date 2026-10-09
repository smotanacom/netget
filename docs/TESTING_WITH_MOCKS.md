# Testing with Mocks

This guide explains how to write E2E tests using the mock LLM infrastructure, enabling fast, reliable tests without requiring a real Ollama instance.

## Table of Contents

- [Quick Start](#quick-start)
- [Why Use Mocks?](#why-use-mocks)
- [Basic Concepts](#basic-concepts)
- [Writing Mock Tests](#writing-mock-tests)
- [Test Modes](#test-modes)
- [Mock Builder API](#mock-builder-api)
- [Common Patterns](#common-patterns)
- [Troubleshooting](#troubleshooting)
- [Examples](#examples)

## Quick Start

```rust
use crate::helpers::*;

#[tokio::test]
async fn test_my_protocol() -> E2EResult<()> {
    let server = start_netget_server(
        ServerConfig::new("Start a TCP server on port 0")
            .with_mock(|mock| {
                mock
                    // Mock 1: User command → open_server action
                    .on_instruction_containing("TCP server")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "TCP",
                        "instruction": "Echo server"
                    }]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Connection event → send_data action
                    .on_event("tcp_connection_received")
                    .respond_with_actions(serde_json::json!([{
                        "type": "send_tcp_data",
                        "data": "48454c4c4f" // "HELLO" in hex
                    }]))
                    .expect_calls(1)
                    .and()
            })
    ).await?;

    // Test your protocol...

    // MANDATORY: Verify all mocks were called correctly
    server.verify_mocks().await?;

    server.stop().await?;
    Ok(())
}
```

Run with:
```bash
# Using test-e2e.sh (mocks are the default)
./test-e2e.sh tcp

# Or with cargo test
cargo test --no-default-features --features tcp --test server -- server::tcp::e2e_test

# To use real Ollama instead:
./test-e2e.sh --use-ollama tcp
# Or:
NETGET_USE_OLLAMA=1 cargo test --no-default-features --features tcp --test server -- server::tcp::e2e_test
```

## Why Use Mocks?

| Aspect | With Ollama | With Mocks |
|--------|-------------|------------|
| **Speed** | 2-10 seconds per LLM call | Milliseconds |
| **Reliability** | Network dependencies | 100% deterministic |
| **CI/CD** | Requires Ollama service | No dependencies |
| **Cost** | GPU/CPU resources | Negligible |
| **Debugging** | LLM responses vary | Predictable behavior |

**When to use mocks:**
- ✅ Unit-style E2E tests (testing specific flows)
- ✅ CI/CD pipelines
- ✅ Rapid development iteration
- ✅ Testing error conditions

**When to use real Ollama:**
- ✅ Validating LLM instruction understanding
- ✅ Integration testing with real model
- ✅ Prompt engineering validation

## Basic Concepts

### Mock Flow

1. **User input** → Mock matches instruction → Returns `open_server` action
2. **Network event** → Mock matches event type → Returns protocol action
3. **Verification** → Check all expectations met (call counts, etc.)

### Mock Structure

Each mock rule has:
- **Matcher**: Criteria to match (instruction substring, event type, etc.)
- **Response**: Actions to return (open_server, send_data, etc.)
- **Expectations**: Call count requirements (exact, min, max)

### Test Lifecycle

```
1. Configure mocks with .with_mock()
2. Start server/client (passes mocks via environment variable)
3. NetGet process uses mocks instead of Ollama
4. Test assertions
5. Call .verify_mocks() ← MANDATORY
6. Stop server/client
```

## Writing Mock Tests

### Server Tests

```rust
#[tokio::test]
async fn test_server_protocol() -> E2EResult<()> {
    let server = start_netget_server(
        ServerConfig::new("Your prompt here")
            .with_mock(|mock| {
                mock
                    // First mock: Handle user command
                    .on_instruction_containing("keyword")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "TCP",
                        "instruction": "Server instruction"
                    }]))
                    .expect_calls(1)
                    .and()
                    // Add more mocks...
            })
    ).await?;

    // Your test logic here

    server.verify_mocks().await?;  // MANDATORY
    server.stop().await?;
    Ok(())
}
```

### Client Tests

```rust
#[tokio::test]
async fn test_client_protocol() -> E2EResult<()> {
    // Start server first
    let server = start_netget_server(
        ServerConfig::new("Start echo server")
            .with_mock(|mock| {
                mock.on_instruction_containing("echo")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "TCP",
                        "instruction": "Echo back data"
                    }]))
                    .expect_calls(1)
                    .and()
            })
    ).await?;

    // Start client with mocks
    let client = start_netget_client(
        NetGetConfig::new(format!("Connect to 127.0.0.1:{}", server.port))
            .with_mock(|mock| {
                mock
                    .on_instruction_containing("Connect")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_client",
                        "remote_addr": format!("127.0.0.1:{}", server.port),
                        "protocol": "TCP",
                        "instruction": "Send data"
                    }]))
                    .expect_calls(1)
                    .and()
            })
    ).await?;

    // Test logic...

    server.verify_mocks().await?;  // MANDATORY
    client.verify_mocks().await?;  // MANDATORY

    server.stop().await?;
    client.stop().await?;
    Ok(())
}
```

## Test Modes

By default, tests use mocks (no Ollama required). Use `--use-ollama` flag to test with real Ollama.

### Mock Mode (Default - Recommended for CI)

```bash
# Using test-e2e.sh
./test-e2e.sh tcp

# Or with cargo test
cargo test --no-default-features --features tcp
```

- ✅ Only uses mocks
- ✅ Fails if mocks not configured
- ✅ Fast and deterministic (milliseconds)
- ✅ No Ollama required

### Real Mode (with --use-ollama)

```bash
# Using test-e2e.sh
./test-e2e.sh --use-ollama tcp

# Or with cargo test
NETGET_USE_OLLAMA=1 cargo test --no-default-features --features tcp
```

- ✅ Only uses real Ollama
- ✅ Fails if Ollama unavailable
- ✅ Validates actual LLM behavior
- ⚠️ Slower (2-10 seconds per LLM call)

## Mock Builder API

### Matchers

Match on different criteria:

```rust
mock
    // Match instruction substring (case-sensitive)
    .on_instruction_containing("TCP")

    // Match event type
    .on_event("tcp_connection_received")

    // Match instruction regex
    .on_instruction_regex(r"start.*server")

    // Match any (fallback)
    .on_any()

    // Combine multiple criteria (AND logic)
    .on_instruction_containing("server")
    .and_instruction_containing("TCP")
    .and_event_data_contains("port", "8080")
```

### Responses

Return actions or raw strings:

```rust
mock
    // Return action JSON
    .respond_with_actions(serde_json::json!([
        {
            "type": "open_server",
            "port": 0,
            "base_stack": "TCP",
            "instruction": "Echo server"
        }
    ]))

    // Return multiple actions
    .respond_with_actions(serde_json::json!([
        {"type": "action1", ...},
        {"type": "action2", ...}
    ]))

    // Return raw string (for testing error handling)
    .respond_with_raw("Invalid JSON")
```

### Expectations

Set call count requirements:

```rust
mock
    // Expect exact count (fails if not met)
    .expect_calls(1)

    // Expect at least N calls
    .expect_at_least(1)

    // Expect at most N calls
    .expect_at_most(3)

    // Combine min and max
    .expect_at_least(1)
    .expect_at_most(5)
```

### Chaining

Build multiple rules:

```rust
mock
    // Rule 1
    .on_instruction_containing("start")
    .respond_with_actions(...)
    .expect_calls(1)
    .and()  // ← Finish this rule, start next

    // Rule 2
    .on_event("connection")
    .respond_with_actions(...)
    .expect_calls(1)
    .and()

    // Rule 3 (last rule can omit .and())
    .on_event("data_received")
    .respond_with_actions(...)
    .expect_calls(1)
```

## Common Patterns

### Pattern 1: Server Startup + Event Handling

```rust
.with_mock(|mock| {
    mock
        // User command → Start server
        .on_instruction_containing("server")
        .respond_with_actions(serde_json::json!([{
            "type": "open_server",
            "port": 0,
            "base_stack": "TCP",
            "instruction": "Handle connections"
        }]))
        .expect_calls(1)
        .and()
        // Connection event → Send response
        .on_event("tcp_connection_received")
        .respond_with_actions(serde_json::json!([{
            "type": "send_tcp_data",
            "data": "response_in_hex"
        }]))
        .expect_calls(1)
        .and()
})
```

### Pattern 2: Multiple Event Handlers

```rust
.with_mock(|mock| {
    mock
        .on_instruction_containing("server")
        .respond_with_actions(serde_json::json!([...]))
        .expect_calls(1)
        .and()
        // First connection
        .on_event("tcp_connection_received")
        .and_iteration(1)  // First LLM iteration
        .respond_with_actions(serde_json::json!([...]))
        .expect_calls(1)
        .and()
        // Data received
        .on_event("tcp_data_received")
        .respond_with_actions(serde_json::json!([...]))
        .expect_at_least(1)  // May be called multiple times
        .and()
})
```

### Pattern 3: Client + Server Interaction

```rust
// Server mocks
let server = start_netget_server(
    ServerConfig::new("Echo server")
        .with_mock(|mock| {
            mock
                .on_instruction_containing("echo")
                .respond_with_actions(serde_json::json!([{
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "TCP",
                    "instruction": "Echo back"
                }]))
                .expect_calls(1)
                .and()
                .on_event("tcp_data_received")
                .respond_with_actions(serde_json::json!([{
                    "type": "send_tcp_data",
                    "data": "ECHO_DATA_HEX"
                }]))
                .expect_calls(1)
                .and()
        })
).await?;

// Client mocks
let client = start_netget_client(
    NetGetConfig::new(format!("Connect to 127.0.0.1:{}", server.port))
        .with_mock(|mock| {
            mock
                .on_instruction_containing("Connect")
                .respond_with_actions(serde_json::json!([{
                    "type": "open_client",
                    "remote_addr": format!("127.0.0.1:{}", server.port),
                    "protocol": "TCP",
                    "instruction": "Send data"
                }]))
                .expect_calls(1)
                .and()
                .on_event("tcp_connected")
                .respond_with_actions(serde_json::json!([{
                    "type": "send_tcp_data",
                    "data": "CLIENT_DATA_HEX"
                }]))
                .expect_calls(1)
                .and()
        })
).await?;

// Verify both
server.verify_mocks().await?;
client.verify_mocks().await?;
```

## Troubleshooting

### Error: "Mock mode requires mocks to be configured"

**Cause:** Running in mock mode (default) but no `.with_mock()` configured.

**Fix:**
```rust
// Add .with_mock() to your config
ServerConfig::new("prompt")
    .with_mock(|mock| { ... })
```

### Error: "Mock verification failed: Expected 1 calls, got 0"

**Cause:** Mock rule was configured but never matched/called.

**Debug:**
1. Check instruction extraction - does the prompt contain the substring?
2. Check event type - is the event name correct?
3. Add debug logging: `.with_log_level("debug")`
4. Look for "MOCK MODE: Matching against N rules" in output

**Common issues:**
- Instruction substring is case-sensitive
- Event type must match exactly
- Multiple `.and_instruction_containing()` uses AND logic (all must match)

### Error: "No servers or clients started in netget"

**Cause:** Mock didn't match, so no action was returned.

**Fix:**
1. Simplify the matcher - use broader substring
2. Check the prompt contains your keyword
3. Try `.on_any()` to match everything (debugging)

### Warning: "Client/Server dropped without calling .verify_mocks()"

**Cause:** Forgot to call `.verify_mocks()` before drop.

**Fix:**
```rust
// ALWAYS call verify_mocks before stop
server.verify_mocks().await?;
server.stop().await?;
```

## Examples

### Example 1: Simple TCP Server

```rust
#[tokio::test]
async fn test_tcp_echo() -> E2EResult<()> {
    let server = start_netget_server(
        ServerConfig::new("Start TCP echo server on port 0")
            .with_mock(|mock| {
                mock
                    .on_instruction_containing("echo")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "TCP",
                        "instruction": "Echo all data"
                    }]))
                    .expect_calls(1)
                    .and()
            })
    ).await?;

    // Connect and test
    let mut stream = tokio::net::TcpStream::connect(
        format!("127.0.0.1:{}", server.port)
    ).await?;

    // Test logic...

    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
```

### Example 2: HTTP Server

```rust
#[tokio::test]
async fn test_http_server() -> E2EResult<()> {
    let server = start_netget_server(
        ServerConfig::new("Start HTTP server on port 0")
            .with_mock(|mock| {
                mock
                    .on_instruction_containing("HTTP")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "HTTP",
                        "instruction": "Respond to GET requests"
                    }]))
                    .expect_calls(1)
                    .and()
                    .on_event("http_request_received")
                    .respond_with_actions(serde_json::json!([{
                        "type": "send_http_response",
                        "status": 200,
                        "headers": {"Content-Type": "text/plain"},
                        "body": "Hello"
                    }]))
                    .expect_at_least(1)
                    .and()
            })
    ).await?;

    // Test HTTP requests...

    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
```

### Example 3: AMQP Broker

See `tests/server/amqp/e2e_test.rs` for complete examples.

```rust
#[tokio::test]
async fn test_amqp_broker() -> E2EResult<()> {
    let server = start_netget_server(
        ServerConfig::new("Start AMQP broker on port 0")
            .with_mock(|mock| {
                mock
                    .on_instruction_containing("AMQP")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "AMQP",
                        "instruction": "AMQP broker"
                    }]))
                    .expect_calls(1)
                    .and()
            })
    ).await?;

    // Test AMQP protocol...

    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
```

## Best Practices

1. **Always verify mocks**: Call `.verify_mocks().await?` before `.stop()`
2. **Use specific matchers**: Prefer `.on_event()` over `.on_any()`
3. **Set expectations**: Use `.expect_calls()` to catch unexpected behavior
4. **Keep mocks simple**: One mock per LLM interaction
5. **Test incrementally**: Start with user command mock, add event mocks as needed
6. **Use hex encoding**: For binary data in TCP/protocol tests
7. **Check logs**: Use `.with_log_level("debug")` when debugging
8. **Document complex mocks**: Add comments explaining what each mock does

## Migration Guide

### Converting Existing Tests

**Before (using real Ollama):**
```rust
let server = start_netget_server(
    ServerConfig::new("Start TCP server")
).await?;
```

**After (using mocks):**
```rust
let server = start_netget_server(
    ServerConfig::new("Start TCP server")
        .with_mock(|mock| {
            mock
                .on_instruction_containing("TCP")
                .respond_with_actions(serde_json::json!([{
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "TCP",
                    "instruction": "Server logic"
                }]))
                .expect_calls(1)
                .and()
        })
).await?;

// Add before stop:
server.verify_mocks().await?;
```

## References

- **Implementation**: `src/testing/` - Core mock infrastructure
- **Examples**: `tests/server/amqp/e2e_test.rs` - AMQP tests with mocks
- **Examples**: `tests/server/tcp/test.rs` - TCP FTP greeting test
- **Test Script**: `test-e2e.sh` - Run tests with mode selection
- **Protocol Checklist**: `CLAUDE.md` - Protocol implementation guide
