# Ollama Server E2E Tests

## Overview

End-to-end tests for the Ollama server protocol. Tests spawn the actual NetGet binary with Ollama API prompts and
validate responses using HTTP clients.

## Test Strategy

**Black-box testing**: Tests interact with NetGet as a user would, sending prompts and validating behavior.

**No unit tests**: Protocol implementation is tested end-to-end to ensure LLM integration works correctly.

## LLM Call Budget

**Target**: < 10 LLM calls total for the entire test suite

**Actual**: 4 LLM calls (well under budget)

1. `test_ollama_list_models` - 1 LLM call (server startup)
2. `test_ollama_generate` - 1 LLM call (server startup)
3. `test_ollama_chat` - 1 LLM call (server startup)
4. `test_ollama_invalid_endpoint` - 1 LLM call (server startup)

## Test Cases

### 1. List Models (`test_ollama_list_models`)

**Purpose**: Verify `/api/tags` endpoint returns model list

**Steps**:

1. Start Ollama server on random port
2. Send `GET /api/tags`
3. Validate response format:
    - HTTP 200 OK
    - JSON with `models` array
    - Each model has `name` field

**Runtime**: ~15 seconds (includes server startup)

**LLM Calls**: 1

### 2. Generate Text (`test_ollama_generate`)

**Purpose**: Verify `/api/generate` endpoint generates text

**Steps**:

1. Start Ollama server
2. Send `POST /api/generate` with:
    - `model`: "qwen2.5-coder:0.5b"
    - `prompt`: "Say 'Hello from NetGet Ollama' and nothing else."
    - `stream`: false
3. Validate response:
    - HTTP 200 OK
    - JSON with `model`, `response`, `done` fields
    - `response` is non-empty string
    - `done` is true

**Runtime**: ~30 seconds (includes LLM generation)

**LLM Calls**: 1 (NetGet startup) + actual Ollama generation (not counted - backend)

### 3. Chat Completion (`test_ollama_chat`)

**Purpose**: Verify `/api/chat` endpoint works

**Steps**:

1. Start Ollama server
2. Send `POST /api/chat` with:
    - `model`: "qwen2.5-coder:0.5b"
    - `messages`: Array with one user message
    - `stream`: false
3. Validate response:
    - HTTP 200 OK
    - JSON with `model`, `message`, `done` fields
    - `message.role` is "assistant"
    - `message.content` is non-empty
    - `done` is true

**Runtime**: ~30 seconds

**LLM Calls**: 1

### 4. Invalid Endpoint (`test_ollama_invalid_endpoint`)

**Purpose**: Verify error handling for unknown endpoints

**Steps**:

1. Start Ollama server
2. Send `GET /api/nonexistent`
3. Validate response:
    - HTTP 404 Not Found
    - JSON with `error` field

**Runtime**: ~5 seconds

**LLM Calls**: 1

## Running Tests

**Feature-specific** (recommended):

```bash
./cargo-isolated.sh test --no-default-features --features ollama --test server::ollama::e2e_test
```

**All features** (slow):

```bash
./cargo-isolated.sh test --all-features --test server::ollama::e2e_test
```

## Requirements

- **Ollama backend**: Tests require real Ollama running (usually on localhost:11434)
- **Model**: `qwen2.5-coder:0.5b` must be pulled (`ollama pull qwen2.5-coder:0.5b`)
- **Network**: Tests bind to 127.0.0.1 (localhost only)

## Expected Output

```
=== E2E Test: Ollama List Models ===
Server started on port 54321
Sending GET /api/tags request...
✓ Received HTTP response: 200 OK
Response JSON: {
  "models": [
    {
      "name": "qwen2.5-coder:0.5b",
      ...
    }
  ]
}
✓ Found 3 models
✓ First model: "qwen2.5-coder:0.5b"
✓ Ollama List Models test completed

=== E2E Test: Ollama Generate ===
...
```

## Known Issues

### 1. Ollama Availability

Tests assume Ollama backend is running. If Ollama is down, tests will fail with connection errors.

**Workaround**: Start Ollama before running tests: `ollama serve`

### 2. Model Availability

Tests use `qwen2.5-coder:0.5b` which must be pulled first.

**Workaround**: `ollama pull qwen2.5-coder:0.5b`

### 3. Port Conflicts

Tests use random available ports (`{AVAILABLE_PORT}` placeholder), so port conflicts are rare.

### 4. Timing Sensitivity

LLM generation can be slow. Tests use 30-second timeouts, which should be sufficient for most systems.

**If tests timeout**: Increase timeout in test code or use a faster model.

## Performance

- **Total runtime**: ~80 seconds (4 tests)
- **Per test**: 5-30 seconds
- **Bottleneck**: Ollama LLM generation time
- **Parallelization**: Not recommended (Ollama lock serializes)

## Test Efficiency

**Why < 10 LLM calls is efficient**:

- Tests focus on API endpoints (HTTP layer)
- Each test validates one core scenario
- LLM is used only for server startup (parsing prompt)
- Actual generation is handled by Ollama backend (not counted)

**Trade-offs**:

- ✅ Fast test suite
- ✅ Low LLM cost
- ✅ Good API coverage
- ⚠️ Doesn't test every endpoint combination
- ⚠️ Doesn't test streaming (future enhancement)

## Future Enhancements

1. **Streaming tests**: Test `stream: true` with NDJSON parsing
2. **Model management**: Test `/api/pull`, `/api/create`, `/api/delete`
3. **Embeddings**: Test `/api/embeddings` endpoint
4. **Error cases**: Test malformed JSON, missing fields
5. **Concurrent requests**: Test multiple simultaneous clients
6. **Large payloads**: Test with long prompts/contexts

## Debugging

**Enable verbose logging**:

```bash
RUST_LOG=debug ./cargo-isolated.sh test --features ollama --test server::ollama::e2e_test -- --nocapture
```

**Check NetGet logs**:

```bash
tail -f netget.log
```

**Manual testing**:

```bash
# Start server
./target/debug/netget "Open Ollama on port 11435"

# Test in another terminal
curl http://localhost:11435/api/tags
curl -X POST http://localhost:11435/api/generate \
  -d '{"model":"qwen2.5-coder:0.5b","prompt":"Hello"}'
```
