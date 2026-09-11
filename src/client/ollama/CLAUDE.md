# Ollama Client Implementation

## Overview

The Ollama client connects to Ollama API servers (real or mock) and allows the LLM to control API requests and interpret
responses. This is useful for testing, automation, and LLM-driven interactions with Ollama models.

## Architecture

### HTTP Client

`reqwest`, through `crate::llm::ollama_client::client_for_endpoint_with_timeout`, built **once
per endpoint** on `spawn_blocking` and cached. Every `perform_*` used to call
`reqwest::Client::new()` for each request, which cost two things on every call:

- `Client::builder().build()` sets up the rustls stack and loads the platform root store. On
  macOS that reads the keychain through Security.framework, synchronously and serialised
  across processes; on the async runtime it parks a tokio worker.
- `reqwest` hands the URL host to its resolver unconditionally and `GaiResolver` does not
  special-case a dotted quad, so `http://127.0.0.1:11434` — which is where this client points
  more often than anywhere else — performed a real `getaddrinfo("127.0.0.1")`. Measured at
  8.25 s with ~100 processes asking at once. `client_for_endpoint_with_timeout` applies a
  resolver override when, and only when, the host parses as an `IpAddr`; a hostname is left
  to the resolver, because `/etc/hosts` or split-horizon DNS may legitimately redirect it.

The cache is keyed by endpoint, because the resolver override is per-host.

### Bounds

- **300 s per request.** `reqwest::Client::new()` has no timeout at all, and the
  injected-command loop awaits each request in turn — so one endpoint that accepted the
  connection and never answered wedged the dashboard's `[ send ]` for this client for the life
  of the process. Generous rather than tight, because generation legitimately takes minutes.
- **8 MiB per response.** `response.json()` buffers the whole body with no limit, and
  `/api/generate` with `stream: true` is an arbitrarily long NDJSON stream by design — so the
  endpoint decided how much memory NetGet spent. `json_bounded` refuses as soon as the cap is
  passed.

### The endpoint

`remote_addr`, and nothing else. This protocol declares **no startup parameters**, so there is
no `base_url` to pass and no vendor default to fall back to: a scheme-qualified address is
used as given, and a bare `host:port` has `http://` prepended (reqwest needs an absolute URL,
and a bare `127.0.0.1:11434` failed every request with "relative URL without a base").

That matters more here than it looks. This client is the one that speaks to whatever NetGet
itself uses as a model backend, so "lost the target and fell back to a default" is not a
cosmetic bug — it is the DynamoDB defect in `CLAUDE.md`, aimed at the operator's own Ollama.
`tests/client/ollama/endpoint_targeting_test.rs` pins it against a stub on an ephemeral
loopback port that no default could name.

### API Endpoints

The client supports:

- `GET /api/tags` - List models
- `POST /api/generate` - Text generation
- `POST /api/chat` - Chat completion
- `POST /api/embeddings` - Embeddings (`generate_embeddings`; not "future" — it has an
  executor, an `apply_action` arm and a follow-up arm)

### Library Choices

- **HTTP Client**: `reqwest` (already in dependencies)
- **JSON**: `serde_json` for request/response parsing
- **No Ollama-specific Library**: Direct HTTP calls keep it simple

## Testing honesty

The five mocked tests in `tests/client/ollama/e2e_test.rs` each passed `base_url` in
`startup_params`. This protocol declares no startup parameters, so `StartupParams::new`
rejected the whole `open_client` with "Undeclared startup parameter 'base_url'" and **no
client was ever created** — in any of them. They passed because their only assertion was that
the output contained the word "Ollama", which that error message itself contains.

They now assert the client's own readiness line, which is printed only after `connect()` has
stored the endpoint and registered the command channel, and they point at dead loopback ports
rather than `localhost:11434`. Pointing a test for *this* protocol at 11434 aims it at
whatever real Ollama the machine is running. The `#[ignore]`d `*_real` variants still use
11434 deliberately and call `require_ollama()` first.

## Connection Model

Unlike TCP-based protocols, Ollama client is **connectionless HTTP**:

1. `connect_with_llm_actions()` stores configuration
2. Sets status to `Connected`
3. Spawns monitor task (checks if client was removed)
4. Actual HTTP requests made on-demand via actions

This pattern matches OpenAI client implementation.

## LLM Integration

### Events

**`ollama_connected`** - Client initialized

Parameters:

- `api_endpoint` - Ollama server URL

**`ollama_response_received`** - Response received from API

Parameters:

- `response_type` - Type: "generate", "chat", "models", "error"
- `content` - Response text or error message
- `model` - Model used (if applicable)

### Actions

**Async Actions** (user-triggered):

1. **`send_generate_request`**
   ```json
   {
     "type": "send_generate_request",
     "prompt": "What is the capital of France?",
     "model": "llama2"
   }
   ```

2. **`send_chat_request`**
   ```json
   {
     "type": "send_chat_request",
     "messages": [
       {"role": "user", "content": "Hello!"}
     ],
     "model": "llama2"
   }
   ```

3. **`list_models`**
   ```json
   {
     "type": "list_models"
   }
   ```

4. **`disconnect`**
   ```json
   {
     "type": "disconnect"
   }
   ```

**Sync Actions** (response-triggered):

1. **`send_generate_request`** - Follow-up generation
2. **`wait_for_more`** - Don't take action
3. **`disconnect`** - Close client

### Action Execution

Actions return `ClientActionResult::Custom` with action data:

```rust
Ok(ClientActionResult::Custom {
    name: "send_generate_request".to_string(),
    data: json!({
        "prompt": prompt,
        "model": model,
    }),
})
```

The action executor then calls the appropriate method:

```rust
match result {
    ClientActionResult::Custom { name, data } => {
        match name.as_str() {
            "send_generate_request" => {
                OllamaClientImpl::make_generate_request(
                    client_id,
                    data["prompt"].as_str().unwrap().to_string(),
                    data["model"].as_str().map(|s| s.to_string()),
                    app_state,
                    llm_client,
                    status_tx,
                ).await
            }
            // ...
        }
    }
}
```

### State Management

Client stores configuration in `protocol_data`:

- `default_model` - Default model for requests
- `api_endpoint` - Ollama server URL

This is accessed via `app_state.with_client_mut()`.

### Request Flow

1. **User/LLM triggers action** → `execute_action()` returns `ClientActionResult::Custom`
2. **Executor calls method** → `make_generate_request()` or similar
3. **HTTP request sent** via `reqwest`
4. **Response received** → Parse JSON
5. **LLM called with event** → `ollama_response_received`
6. **LLM decides next action** → More requests, disconnect, or wait

## Limitations

### 1. No Streaming Support

Current implementation sets `"stream": false` in all requests. Streaming would require:

- WebSocket or SSE for server-sent events
- Parsing newline-delimited JSON (NDJSON)
- Incremental LLM calls as chunks arrive

**Future**: Could use `reqwest::Response::bytes_stream()` to handle NDJSON.

### 2. Limited API Coverage

Only implements core endpoints:

- `/api/tags`
- `/api/generate`
- `/api/chat`

Missing:

- `/api/embeddings`
- `/api/pull`
- `/api/show`
- Model management endpoints

**Future**: Add as needed for testing scenarios.

### 3. No Error Recovery

If a request fails, the error is logged but no automatic retry. LLM can decide to retry via actions, but no built-in
backoff/retry logic.

### 4. No Request Cancellation

Long-running generate requests cannot be cancelled mid-flight. Ollama API supports this via DELETE to `/api/generate`,
but not implemented.

## Startup Parameters

```rust
ParameterDefinition {
    name: "default_model",
    description: "Default model to use for requests",
    type_hint: "string",
    required: false,
    example: json!("llama2"),
}
```

Example:

```
open_client ollama http://localhost:11434 "Ask llama2 about Rust" default_model=llama2
```

## Testing Strategy

See `tests/client/ollama/CLAUDE.md` for E2E testing approach.

Key test scenarios:

- Connect to Ollama server
- Send generate request
- Send chat request
- List models
- Handle errors (server down, invalid model)

## Use Cases

1. **LLM-to-LLM**: Use NetGet LLM to control queries to Ollama models
2. **Testing**: Test Ollama servers (real or mock)
3. **Automation**: Automated workflows with Ollama
4. **Multi-Model**: LLM decides which model to use based on task

## Example Prompts

```
Connect to Ollama at http://localhost:11434 and generate a poem about Rust
```

```
Use Ollama to ask llama2: "What is the capital of France?"
```

```
Connect to my local Ollama and list all available models
```

## Performance

- Lightweight HTTP client (reqwest)
- Minimal overhead (just HTTP + JSON)
- LLM call overhead same as other protocols
- Can run multiple clients concurrently

## Future Enhancements

1. **Streaming**: Support streaming responses
2. **Full API**: Implement all Ollama endpoints
3. **Request Cancellation**: Support aborting long requests
4. **Auto-Retry**: Configurable retry logic
5. **Connection Pooling**: Reuse HTTP connections
6. **Custom Endpoints**: Support Ollama API extensions

## Comparison with OpenAI Client

| Feature         | Ollama Client            | OpenAI Client        |
|-----------------|--------------------------|----------------------|
| **API Library** | `reqwest` (direct)       | `async-openai`       |
| **Auth**        | None                     | API key required     |
| **Endpoints**   | 3 (tags, generate, chat) | 2 (chat, embeddings) |
| **Streaming**   | Not yet                  | Not yet              |
| **Complexity**  | Simple                   | Moderate             |
| **Use Case**    | Local models             | Cloud API            |

## Error Handling

Errors are categorized:

1. **Connection Errors**: Server unreachable → Event with `response_type: "error"`
2. **API Errors**: Server returns error JSON → Parsed and sent to LLM
3. **Parse Errors**: Invalid JSON → Logged and sent to LLM as error event

LLM can decide how to handle errors:

- Retry with different model
- Disconnect
- Log and continue

## Injected actions (the dashboard's `[ send ]`)

The client registers a command channel and spawns `OllamaClientImpl::command_loop`
**before** the `ollama_connected` LLM call, because a `*` -> manual rule can park that call
for minutes and the operator must still be able to reach the client. The command task also
replaces the old 5s "has the client been removed yet" poll.

Both the connected-event path and injected commands go through one
`OllamaClientImpl::apply_action`, which maps every `Custom` result to its request function
(`send_generate_request`, `send_chat_request`, `generate_embeddings`, `list_models`). Before
this, the connected-event handler discarded the LLM's actions entirely - it logged "ready
after connect event" and never called `make_generate_request`.

`ClientSendOutcome` semantics:

| Outcome | When |
|---|---|
| `Executed { detail }` | The API call ran to completion, e.g. `send_generate_request completed (model=llama2)`. |
| `Rejected { error }` | `execute_action` refused the action (unknown type, missing required `model`). |
| `Disconnected` | `{"type":"disconnect"}`; the command loop exits and the handle is dropped. |
| `Err(...)` | The HTTP call failed, or Ollama answered with an error body. |

**`Sent { bytes_sent }` is never reported**: reqwest owns the socket, so a byte count would
be invented. The request is awaited before the outcome is returned, so `Executed` means it
really completed and `ollama_response_received` has already fired.
