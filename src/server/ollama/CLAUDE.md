# Ollama Server Implementation

## Overview

The Ollama server provides an LLM-controlled Ollama-compatible API server. Unlike the OpenAI server which passes through
to the real Ollama backend, this Ollama server acts as a mock/test server where the LLM decides how to respond to Ollama
API requests.

## Architecture

### HTTP-based Protocol

Ollama uses a simple HTTP-based REST API with these main endpoints:

- `GET /api/tags` - List available models
- `POST /api/generate` - Text generation
- `POST /api/chat` - Chat completion
- `POST /api/embeddings` - Generate embeddings
- `POST /api/show` - Show model information
- `POST /api/pull` - Pull a model
- `POST /api/create` - Create a model
- `POST /api/copy` - Copy a model
- `DELETE /api/delete` - Delete a model

### Library Choices

- **HTTP Server**: `hyper` v1.x with `http1` connection handling
- **JSON**: `serde_json` for request/response parsing
- **No Client Library**: Unlike OpenAI server which uses `async-openai`, this server constructs responses directly

### Response Format

Ollama API responses use simple JSON format:

```json
// /api/generate response
{
  "model": "llama2",
  "created_at": "2024-01-01T00:00:00Z",
  "response": "The capital of France is Paris.",
  "done": true
}

// /api/chat response
{
  "model": "llama2",
  "created_at": "2024-01-01T00:00:00Z",
  "message": {
    "role": "assistant",
    "content": "Hello! How can I help?"
  },
  "done": true
}
```

## LLM Integration

### Current Implementation (V1 - Direct Response)

The current implementation generates responses using the NetGet LLM (Ollama) directly:

```rust
let response_text = llm_client.generate(&model, &prompt).await?;
```

This means the Ollama server **uses Ollama to respond to Ollama API requests** - a bit meta but functional.

### Future Enhancement (V2 - LLM Control)

A more sophisticated version would:

1. Receive Ollama API request (e.g., `/api/generate`)
2. Call NetGet LLM with event: "ollama_generate_request_received"
3. LLM decides how to respond (via actions):
    - `ollama_generate_response` - Return text
    - `ollama_error_response` - Return error
    - `ollama_models_response` - Return model list
4. Execute action and send HTTP response

This would allow the LLM to:

- Return custom/fake responses for testing
- Simulate errors or edge cases
- Act as a honeypot with controllable behavior
- Test client implementations

### Dual Logging

All operations use dual logging pattern:

- `tracing` macros → `netget.log`
- `status_tx.send()` → TUI

Example:

```rust
debug!("Chat: model={}, {} messages", model, messages.len());
let _ = status_tx.send(format!("[DEBUG] Chat: model={}, {} messages", model, messages.len()));
```

## Connection Tracking

Each HTTP request is treated as a separate connection:

1. Accept TCP connection
2. Create `ConnectionId` and add to `ServerInstance`
3. Serve HTTP request(s) via `hyper`
4. Mark connection as closed when done

Connection state includes:

- Remote address
- Bytes sent/received
- Packets sent/received
- Last activity timestamp

## Limitations

### 1. No Streaming Support

Ollama API supports streaming responses (newline-delimited JSON):

```
{"response":"The","done":false}
{"response":" capital","done":false}
{"response":" of","done":false}
{"response":" France","done":false}
{"response":" is","done":false}
{"response":" Paris","done":false}
{"response":".","done":true}
```

Current implementation uses `"stream": false` and returns full response at once.

**Future**: Could implement streaming by:

- Using `hyper::body::Body` with `SyncSender<Result<Bytes, Infallible>>`
- LLM generates chunks via actions
- Stream chunks back to client

### 2. Model Management is a decision, not a rubber stamp

`/api/pull`, `/api/create`, `/api/copy` and `/api/delete` perform nothing — there is no model
store here, by design. What changed in August 2026 is **who decides what to report**.

They used to answer `{"status": "success"}` unconditionally, with no event and no `call_llm`
anywhere in the path, and `/api/pull` invented a digest of `sha256:0000000000000000`. So a
server instructed "this instance only serves llama2, refuse anything else" reported every pull
as downloaded and every delete as removed: the instruction could not be wrong, it simply had no
effect. That is the fail-open shape from the root `CLAUDE.md` in its purest form — the decision
was never asked for.

All four now raise **`ollama_admin_request`** (`operation`, `model`, `destination`) and require
an explicit **`ollama_admin_ok`** to report success. `ollama_error_response` refuses with the
model's own message and status. Three outcomes refuse, kept apart in the log:
`decision=model_reject`, `decision=fail_closed_no_action`, `decision=fail_closed_llm_error` —
the last two carry only a `WireFailure` category, never the backend error.

`ollama_admin_ok` may carry `digest` and `total`, which `/api/pull` echoes. Nothing is invented:
omit them and the reply is just `{"status": "success"}`.

`/api/show` is now the model's answer too, via **`ollama_show_request`** / **`ollama_show_response`**
(modelfile, parameters, template, details — all optional; no action means the request is
refused). It used to reply with a fabricated Modelfile (`FROM {name}`), a hardcoded
`temperature 0.7` and a `gguf`/`llama` details block, for any name at all — so a server told
"this instance serves only llama2" cheerfully described every model a client asked about,
including ones it had just refused to pull.

`/api/embeddings` was the last endpoint still answering without asking, and the argument that
defended it was wrong in one load-bearing detail. The argument: an embedding is a few hundred
to a few thousand floats, asking a language model to emit them would produce plausible-looking
noise, and that is the numeric equivalent of the raw-bytes-in-actions rule the root
`CLAUDE.md` forbids. All true. The conclusion it drew — "if an operator ever needs real
control here the answer is a script handler, not an action" — was not: **the endpoint raised
no event, so a script handler could not reach it either.** Neither could a static rule, nor
the instruction, nor anything else an operator can write. It was not a stub with an escape
hatch; it was a hardcoded 768-element ramp with no way in at all.

It now raises **`ollama_embeddings_request`** (`model`, `prompt`) and answers only on
**`ollama_embeddings_response`**, refusing otherwise — the same shape as `/api/show`. The
float problem is solved by not asking for floats: the action takes `dimensions` and the
executor builds the ramp, so what the model decides is *whether* to embed and *how wide*, not
what the numbers are. `embedding` remains available for a handler that does have a real
vector. Both are bounded at 4096 elements, because the vector is serialised into the reply and
the width is model-supplied.

A `dimensions`-only answer is still numerically meaningless, and both the action description
and this paragraph say so. That is a different thing from meaninglessness nobody chose.

### 3. Embeddings are shaped, not computed

`/api/embeddings` returns a deterministic ramp of the width the handler asked for. Producing a
vector that actually encodes the prompt would need a real embedding model behind NetGet; there
is none, and nothing here pretends otherwise. What the handler controls is the decision and
the shape.

### 4. No Authentication

Real Ollama has no auth, but a production mock server might want API keys for access control.
Note that this makes every endpoint below reachable pre-auth, which is why the body cap
matters.

### 5. Request bodies are capped at 8 MiB

`hyper`'s `Incoming` has no default limit, so `req.collect()` buffers whatever the peer sends.
Every endpoint here is unauthenticated, so a single `POST /api/generate` with an endless
chunked body was enough to walk the process out of memory. `read_body_limited` uses
`http_body_util::Limited`, which errors as soon as the cap is passed rather than after
buffering, and answers 413. The cap is deliberately small: the body is parsed and then
embedded in an LLM prompt, and a model cannot read 8 MiB.

A distinct 413 rather than an empty body is the point — a truncated body handed to the model
looks like a complete one, and the model would answer a request it never saw. The constant is
declared locally rather than imported from `http_common`, because the `ollama` feature does
not pull in `http` and that module is configured out of an `--features ollama` build.

## A refusal must not arrive as a success

`model_error_response` is the only path that turns `ollama_error_response` into an HTTP reply,
and it read the model's `status_code` with `StatusCode::from_u16(code as u16)`. `as u16` wraps:
`65736` narrows to `200`, `from_u16(200)` succeeds, and the refusal reached the client as a
200 whose body happens to carry an `error` key — which every Ollama client reads as an answered
request. The function's own doc comment already said a refusal and an outage must stay
distinguishable; arithmetic was quietly erasing a third distinction, refusal versus success.

The conversion is now checked, the range is 100–599 (not `StatusCode`'s own 100–999 — 600+ is
syntactically valid and means nothing), and a 2xx is rejected even when written literally,
because this path only ever builds refusals. Anything unusable falls back to 400 with a WARN
rather than failing the request: the refusal itself is the part that must survive.
`tests/server/ollama/refusal_status_test.rs` pins all four cases.

## Testing Strategy

See `tests/server/ollama/CLAUDE.md` for E2E testing approach.

Key test scenarios:

- List models
- Generate text
- Chat completion
- Error handling
- Invalid requests

## Use Cases

1. **Client Testing**: Test Ollama clients against controlled server
2. **Honeypot**: LLM-controlled fake Ollama server
3. **Protocol Development**: Experiment with Ollama API extensions
4. **Network Simulation**: Simulate Ollama in isolated environments

## Performance

- Lightweight HTTP server (hyper)
- No heavy dependencies
- LLM call overhead same as other protocols
- Can handle multiple concurrent connections

## Future Enhancements

1. **Streaming**: Implement streaming responses
2. **LLM Control**: Let LLM decide all responses (not just delegate to Ollama)
3. **Model State**: Track "pulled" models in memory
4. **Custom Endpoints**: Support Ollama API extensions
5. **Metrics**: Track request counts, response times, etc.

## Example Prompts

```
Start an Ollama-compatible API server on port 11435
```

```
Run an Ollama server on 0.0.0.0:11435 that always returns funny responses
```

```
Create a fake Ollama server for testing on port 8080
```
