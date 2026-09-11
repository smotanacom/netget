# OpenAI Client Protocol Implementation

## Overview

The OpenAI client provides LLM-controlled access to the OpenAI API, enabling chat completions, embeddings generation,
and other AI capabilities. This implementation uses the `async-openai` crate (v0.26) which provides a well-tested Rust
SDK for the OpenAI API.

## Library Choices

### Primary Library: `async-openai`

**Crate:** `async-openai` v0.26
**Repository:** https://github.com/64bit/async-openai
**License:** MIT

**Why async-openai:**

- **Official-like quality**: Well-maintained, follows OpenAI API specifications closely
- **Async-first**: Built on tokio for efficient async operations
- **Type-safe**: Strongly-typed request/response structures
- **Complete coverage**: Supports chat completions, embeddings, and function calling
- **Active maintenance**: Regular updates to match OpenAI API changes
- **Streaming support**: Can handle streaming responses (future enhancement)

**Alternatives considered:**

- **Direct HTTP calls with reqwest**: More flexible but requires manual request building and error handling
- **openai-api-rust**: Less actively maintained, fewer features

## Architecture

### Connection Model

Unlike traditional network clients, the OpenAI client is "connectionless" - it's an HTTP API client that makes requests
on demand. The connection process:

1. **Initialization**: Store API credentials in client protocol data
2. **Background task**: Monitor for client lifecycle (connection removal)
3. **On-demand requests**: API calls triggered by LLM actions

### Client State Machine

```
┌─────────────┐
│   Created   │
└──────┬──────┘
       │ connect()
       ▼
┌─────────────┐
│  Connected  │ ◄─── API key validated
└──────┬──────┘
       │ LLM actions trigger API calls
       ▼
┌─────────────┐
│  Processing │ ◄─── Making OpenAI API request
└──────┬──────┘
       │ Response received
       ▼
┌─────────────┐
│   Response  │ ◄─── Call LLM with response event
└─────────────┘
```

### LLM Integration

**Events:**

1. **`openai_connected`**: Fired when client initializes
    - Parameters: `api_endpoint`

2. **`openai_response_received`**: Fired when API responds
    - Parameters: `response_type`, `content`, `model`, `usage`

**Actions:**

1. **`send_chat_completion`**: Create chat completion
    - Parameters: `messages`, `model`, `temperature`, `max_tokens`, `functions`

2. **`send_embedding_request`**: Generate embeddings
    - Parameters: `input`, `model`

3. **`disconnect`**: Close client (stop monitoring)

### Request Flow

```
User Instruction
     │
     ▼
LLM decides action (send_chat_completion)
     │
     ▼
Action parsed → execute_action()
     │
     ▼
Custom result: openai_chat_completion
     │
     ▼
Main event loop → make_chat_completion()
     │
     ▼
async-openai → OpenAI API
     │
     ▼
Response received
     │
     ▼
Event: openai_response_received
     │
     ▼
LLM processes response
```

### Startup Parameters

- **`api_key`** (required): OpenAI API key (sk-...)
- **`default_model`** (optional): Default model to use (default: gpt-3.5-turbo)
- **`organization`** (optional): OpenAI organization ID
- **`api_endpoint`**: not a startup parameter. The API base is `remote_addr`, resolved once by
  `api_base_for` and stored in `protocol_data`. There is **no default**: an empty
  `remote_addr` refuses to connect.

  That refusal is the point. `async-openai`'s config defaults to `https://api.openai.com/v1`,
  and this client used to leave the config alone unless `remote_addr` was non-empty *and* not
  literally that string — so an unrecorded address meant presenting the key to real OpenAI.
  It is the DynamoDB shape from the root `CLAUDE.md`: a client that loses its target and
  reaches the real service instead of failing. Pointing this client at real OpenAI is a
  legitimate request; arriving there because nobody said otherwise is not.

  A scheme is used as given. Without one, `https://` is assumed except for a loopback host,
  where `http://` is the only thing that could be meant — `reqwest` needs an absolute URL, and
  a bare `api.openai.com:443` failed every request rather than doing anything useful. The base
  must include the version path; `async-openai` appends only the operation to it.

### The API key

Read from `startup_params`, stored in `protocol_data`, and handed to `OpenAIConfig::with_api_key`.
It is not interpolated into any log line, event payload or status message, and `organization`
is treated the same way. Nothing on the response path echoes a request header.

### Bounds on model-supplied numbers

`max_tokens` had **two** narrowing casts between the model and the wire — `as u32` in the
executor and `as u16` where the request was built — so `max_tokens: 70000` was sent as 4464.
A budget silently replaced by a smaller one reads on the wire as a deliberate choice, which is
the worst way for a cast to fail. It is now range-checked (1 to 1,000,000) and refused out of
range, and `temperature` is checked against OpenAI's own 0.0-2.0 and refused if not finite.

### Timeout

300s per call. `async-openai` builds a `reqwest::Client::new()` when not given one, which has
no timeout at all, and the injected-command loop awaits each request in turn — so an endpoint
that accepted the connection and never answered wedged the dashboard's `[ send ]` for this
client for the life of the process.

## Implementation Details

### Chat Completions

**Message Format:**

```json
{
  "type": "send_chat_completion",
  "messages": [
    {"role": "system", "content": "You are a helpful assistant"},
    {"role": "user", "content": "Hello!"}
  ],
  "model": "gpt-4",
  "temperature": 0.7,
  "max_tokens": 150
}
```

**Supported Roles:**

- `system`: System message (context setting)
- `user`: User message
- `assistant`: Assistant message (conversation history)

**Response Structure:**

- Extracts first choice content
- Includes token usage stats (prompt_tokens, completion_tokens, total_tokens)
- Sends structured event to LLM for processing

### Embeddings

**Request Format:**

```json
{
  "type": "send_embedding_request",
  "input": "Text to embed",
  "model": "text-embedding-ada-002"
}
```

**Input Types:**

- Single string: `"input": "text"`
- Array of strings: `"input": ["text1", "text2"]`

**Response:**

- Returns embedding count and dimensions
- Stores embeddings in client memory (future: expose to LLM)

### Error Handling

All API errors are:

1. Logged via tracing
2. Sent to status channel for UI display
3. Wrapped in `openai_response_received` event with `response_type: "error"`
4. Passed to LLM for potential retry logic

## Limitations

### Current Limitations

1. **Function Calling**: Declared in action parameters but not yet implemented
    - Requires translating OpenAI function schemas to/from LLM action format
    - Future enhancement planned

2. **Streaming**: async-openai supports streaming, but not integrated
    - Would require modifying event system to handle partial responses
    - Future enhancement for long-running completions

3. **Embeddings Storage**: Embeddings are generated but not stored long-term
    - Could be enhanced with vector database integration
    - LLM currently only sees embedding count/dimensions

4. **Model Validation**: No validation of model names
    - Invalid models fail at API call time
    - Could add model enumeration

5. **Custom Endpoints**: Basic support for alternative OpenAI-compatible APIs
    - Tested primarily with official OpenAI API
    - May require adjustments for some providers

### Protocol-Specific Considerations

- **Rate Limits**: No built-in rate limiting (relies on OpenAI API backpressure)
- **Token Costs**: No cost tracking (token usage reported but not accumulated)
- **API Key Security**: Stored in protocol_data (in-memory only, not persisted)

## Testing Strategy

See `tests/client/openai/CLAUDE.md` for testing details.

**E2E Test Requirements:**

- Valid OpenAI API key (set via startup params)
- Network access to OpenAI API
- Budget for API token usage (minimal, <1000 tokens per test)

**Test Coverage:**

- Basic chat completion (1 LLM call)
- Multi-turn conversation (2-3 LLM calls)
- Embeddings generation (1 LLM call)
- Error handling (invalid model, missing API key)

## Future Enhancements

1. **Function Calling**: Full support for OpenAI function calling
2. **Streaming**: Streaming response handling
3. **Vision API**: Image inputs for GPT-4 Vision
4. **Audio API**: Whisper (transcription) and TTS (text-to-speech)
5. **Fine-tuning**: Support for fine-tuned model management
6. **DALL-E**: Image generation via DALL-E API
7. **Embeddings Database**: Vector storage integration
8. **Cost Tracking**: Accumulate token usage and estimated costs

## Example Prompts

```
# Basic chat completion
"Connect to OpenAI with key sk-... and ask GPT-4 to explain quantum computing"

# Multi-turn conversation
"Connect to OpenAI and have a conversation about AI ethics, asking follow-up questions based on responses"

# Embeddings
"Connect to OpenAI and generate embeddings for the text: 'Machine learning is a subset of artificial intelligence'"

# Custom endpoint (OpenAI-compatible)
"Connect to https://api.example.com/v1 with OpenAI client and make a chat completion"
```

## Dependencies

**Runtime:**

- `async-openai` v0.26 (OpenAI API client)
- `tokio` (async runtime)
- `serde_json` (JSON serialization)
- `anyhow` (error handling)
- `tracing` (logging)

**Dev:**

- `async-openai` v0.26 (E2E tests)

## References

- **OpenAI API Docs**: https://platform.openai.com/docs/api-reference
- **async-openai Crate**: https://docs.rs/async-openai/
- **OpenAI Models**: https://platform.openai.com/docs/models

## Injected actions (the dashboard's `[ send ]`)

The client registers a command channel (`command_support::register_command_channel`) and
spawns `OpenAiClient::command_loop` **before** the `openai_connected` LLM call. That order
matters: a dashboard-created client defaults to a `*` -> manual rule, so the connect call
can park for minutes waiting for a human, and registering afterwards would leave `[ send ]`
reading "no command channel" for the whole park. The command task also replaces the old 5s
"has the client been removed yet" poll - the channel closes when the client is removed.

Both the connected-event path and injected commands go through one
`OpenAiClient::apply_action`, so an injected `send_chat_completion` is byte-for-byte the
same request the LLM would have made. (Before this, the connected-event handler discarded
the LLM's actions entirely: it logged "ready after connect event" and never called
`make_chat_completion`.)

`ClientSendOutcome` semantics - these are deliberate, not placeholders:

| Outcome | When |
|---|---|
| `Executed { detail }` | The API call ran to completion. `detail` names the verb and model, e.g. `send_chat_completion completed (model=gpt-4)`. |
| `Rejected { error }` | `execute_action` refused the action (unknown type, missing `messages`). |
| `Disconnected` | `{"type":"disconnect"}`; the command loop exits and the handle is dropped. |
| `Err(...)` | The HTTP/API call itself failed (transport error, OpenAI error response). |

**`Sent { bytes_sent }` is never reported, and that is correct.** The OpenAI client owns no
socket - `async-openai` owns the connection and does TLS framing - so any byte count would
be invented. The injected request *is* awaited before the outcome is returned, so
`Executed` means the request really completed and the `openai_response_received` event has
already fired. The connect-event path still dispatches its requests to spawned tasks, so a
slow API call cannot hold up `connect()`.
