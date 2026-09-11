# OpenAI-Compatible API Server Implementation

## Overview

OpenAI-compatible HTTP API server that wraps Ollama, allowing clients to use OpenAI libraries and tools to interact with
local LLM models. Implements the OpenAI API specification for model listing and chat completions.

## Protocol Version

- **OpenAI API**: v1 (compatible with OpenAI Python SDK, async-openai Rust client)
- **Endpoints**: `/v1/models`, `/v1/chat/completions`
- **Transport**: HTTP/1.1 with JSON request/response bodies

## Library Choices

### Core Dependencies

- **hyper** (v1) - HTTP/1.1 server implementation
    - Chosen for: async/await support, efficient connection handling
    - Used for: HTTP request/response processing
- **http-body-util** - HTTP body utilities
    - Chosen for: body collection and Full body type
- **serde_json** - JSON serialization/deserialization
    - Chosen for: OpenAI API format compliance
- **tokio** - Async runtime
    - Chosen for: concurrent connection handling

### Why Not Use an OpenAI Server Library?

- No suitable Rust library exists for *building* OpenAI-compatible servers
- Implementing directly gives full control over LLM integration
- Simple API surface (2 endpoints) doesn't justify additional dependencies

## Architecture Decisions

### LLM Integration Approach

**Event/Action System** - The OpenAI server uses NetGet's event/action pattern:

1. Receives OpenAI-format HTTP requests
2. Emits `openai_request` event with method, path, and body
3. LLM generates appropriate actions (openai_chat_response, openai_models_response, etc.)
4. Actions build OpenAI-compatible responses

This approach provides:

- Full LLM control over responses
- Consistent with other NetGet protocols
- Extensible action system

### Actions and Events

**Event**: `openai_request`
- Parameters: `method`, `path`, `body`
- Emitted for all OpenAI API requests

**Actions**:
- `openai_models_response`: Returns model list in OpenAI format
- `openai_chat_response`: Returns chat completion in OpenAI format
- `openai_error_response`: Returns error in OpenAI format

The LLM receives the request event and decides which action to return based on the path and method.

### Connection Management

- Each HTTP connection spawned as separate tokio task
- Connections tracked in `ProtocolConnectionInfo::OpenAi` with `recent_requests` Vec
- HTTP/1.1 keep-alive handled by hyper's `serve_connection`
- No manual connection cleanup needed (hyper handles closing)

### Action System Integration

**Every request is a model decision.** This section used to say the opposite — "most logic is
hardcoded (no LLM prompting needed)" and "empty action lists" — and none of it was true:
`handle_openai_request` calls `call_llm` for every request that gets past routing, and
`get_sync_actions()` returns all three response actions. Nothing here answers without asking.

The three actions the model may return are `openai_chat_response`, `openai_models_response`
and `openai_error_response`, and all three are attached to `openai_request` with
`.with_actions(...)` — without that, `call_llm` would offer the model nothing protocol-specific
and reject every answer it produced.

## State Management

### Per-Connection State

None. `ProtocolConnectionInfo` is a generic `serde_json::Value` wrapper rather than an enum
(see `state/server.rs`), and this server stores `ProtocolConnectionInfo::empty()`. The
`ProtocolConnectionInfo::OpenAi { recent_requests }` variant this section used to show has
never existed.

### No Session State

- Each request is stateless (true to OpenAI API design)
- No conversation history maintained server-side
- Client provides full message history in each request

## Limitations

### Not Implemented

- **Streaming responses** - No SSE support (OpenAI SDK supports `stream: true`)
- **Function calling** - Tools/function_call parameters ignored
- **Embeddings endpoint** - Only chat completions supported
- **Fine-tuning endpoints** - Not applicable to Ollama models
- **API key authentication** - none. The server does not read request headers at all, so an
  `Authorization` header is neither validated nor shown to the model — which also means it is
  never logged, never put in an event and never reaches the status stream. For a server whose
  whole job is to look like an LLM backend, a client pointed at it presents a real key, so
  *not* capturing it is the safer default; making it a model decision would mean deliberately
  putting a credential into a prompt.

### Bounds

- **Request bodies are capped at 8 MiB** and refused with 413 in OpenAI's own error envelope.
  `Incoming` has no default limit and this endpoint is unauthenticated, so `req.collect()`
  used to buffer whatever the peer sent; the body is then handed to the model as prompt text,
  where a megabyte is already useless. A distinct 413 rather than an empty body matters,
  because a truncated body looks complete to the model.
- **The response builder cannot panic.** `status` was read with `as u16`, and headers from
  action data were applied with `.unwrap()` at the end — a header value containing CR/LF (a
  response-splitting attempt) or a malformed name made the builder return `Err` and panicked
  the connection task. Individual bad headers are dropped and a status outside 100-599 becomes
  a 500 rather than wrapping toward 200.

### Failure semantics

A backend failure answers 503 when `WireFailure` classifies it as overloaded and 500
otherwise, so a client backs off rather than recording a permanent fault. The peer gets a
category; the log gets the error, tagged `decision=fail_closed_llm_error` or
`decision=fail_closed_no_action`. The old no-action path answered "LLM did not return valid
response", which is netget's own internals on a stranger's terminal.

### Response Format Compromises

- Token usage is always `{prompt_tokens: 0, completion_tokens: 0, total_tokens: 0}`
    - Ollama doesn't expose token counts in generate API
- Model parameter may not match requested model
    - Falls back to app_state default model if not specified
- Temperature/max_tokens parameters passed but not validated
    - Ollama may interpret differently than OpenAI

### Ollama-Specific Behavior

- Model names follow Ollama format (e.g., `qwen2.5-coder:0.5b`)
- Response timing may differ from OpenAI (local inference)
- Availability depends on Ollama service running

## Example Prompts and Responses

### Startup (No Prompting Needed)

```bash
netget "open_server port 11435 base_stack openai"
```

The server starts immediately with full OpenAI compatibility. No LLM instructions needed.

### Client Usage (Python)

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:11435/v1",
    api_key="dummy"  # Not validated
)

# List models
models = client.models.list()
for model in models.data:
    print(model.id)

# Chat completion
response = client.chat.completions.create(
    model="qwen2.5-coder:0.5b",
    messages=[
        {"role": "user", "content": "Hello!"}
    ]
)
print(response.choices[0].message.content)
```

### Client Usage (Rust)

```rust
use async_openai::{Client, types::*};

let config = OpenAIConfig::new()
    .with_api_base("http://127.0.0.1:11435/v1")
    .with_api_key("dummy");
let client = Client::with_config(config);

// List models
let models = client.models().list().await?;

// Chat completion
let request = CreateChatCompletionRequestArgs::default()
    .model("qwen2.5-coder:0.5b")
    .messages(vec![
        ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("Hello!")
                .build()?
        )
    ])
    .build()?;
let response = client.chat().create(request).await?;
```

## References

- [OpenAI API Reference](https://platform.openai.com/docs/api-reference)
- [Ollama API Documentation](https://github.com/ollama/ollama/blob/main/docs/api.md)
- [async-openai Rust Client](https://github.com/64bit/async-openai)
- [OpenAI Python SDK](https://github.com/openai/openai-python)

## Key Design Principles

1. **Every response is the model's** - there is no hardcoded answer path; a server that
   cannot reach its backend refuses rather than inventing a completion
2. **Real Responses** - composed by netget's own LLM backend, not canned. Note the confusion
   this family invites: the backend netget *talks to* and the backend this server *pretends to
   be* are different things, and the peer's prompt reaching netget's model is by design
3. **Full Compatibility** - Works with standard OpenAI SDKs
4. **Minimal Translation** - Thin layer between OpenAI API and Ollama
5. **No State** - Stateless design matches OpenAI API philosophy
