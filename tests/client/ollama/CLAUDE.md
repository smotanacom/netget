# Ollama Client E2E Tests

## Overview

End-to-end tests for the Ollama client protocol. Tests spawn NetGet binary as a client and verify it can interact with
Ollama servers.

## Test Strategy

**Black-box testing**: Tests interact with NetGet client through prompts and observe output.

**No real Ollama is contacted by any test that runs.** The five mocked tests point at dead
loopback ports on purpose: this protocol *is* an Ollama client, so aiming a test at
`localhost:11434` aims it at whatever real Ollama the machine is running. Only the
`#[ignore]`d `*_real` variants want one, and each calls `require_ollama()` first.

**These five tests asserted nothing until September 2026.** Each passed `base_url` in
`startup_params`; the Ollama client declares no startup parameters, so every `open_client`
was rejected with "Undeclared startup parameter 'base_url'" and no client was ever created.
The only assertion was that the output contained "Ollama" — which that error message
contains. They now assert `ready (endpoint: ...)`, printed only after `connect()` succeeded.

Two suites carry the real coverage: `endpoint_targeting_test` (all four verbs reach the
endpoint the operator named, and an endless response body is refused rather than buffered)
and `command_channel_test` (the dashboard's injected-action path against a loopback stub).
Neither needs a mock LLM at all - a `*` static handler with no actions answers every client
event.

## LLM Call Budget

**Target**: < 10 LLM calls total

**Actual**: 5 LLM calls (well under budget)

1. `test_ollama_client_list_models` - 1 LLM call
2. `test_ollama_client_generate` - 1 LLM call
3. `test_ollama_client_chat` - 1 LLM call
4. `test_ollama_client_custom_endpoint` - 1 LLM call
5. `test_ollama_client_error_handling` - 1 LLM call

## Test Cases

### 1. List Models (`test_ollama_client_list_models`)

**Purpose**: Verify client can list available models

**Steps**:

1. Start NetGet client with prompt to list models
2. Wait for client to connect and make request
3. Validate output:
    - Shows "Ollama" protocol
    - Shows "models" or "model" or "found" in response
4. Stop client

**Runtime**: ~15 seconds

**LLM Calls**: 1

**Requirements**: Ollama server running on localhost:11434

### 2. Generate Text (`test_ollama_client_generate`)

**Purpose**: Verify client can generate text

**Steps**:

1. Start client with prompt to generate text
2. Wait for generation (up to 30s)
3. Validate output:
    - Shows "Ollama" protocol
    - Shows "response" or "generate" or "received"
4. Stop client

**Runtime**: ~30 seconds

**LLM Calls**: 1 (NetGet) + Ollama backend (not counted)

**Requirements**:

- Ollama server running
- Model `qwen2.5-coder:0.5b` available

### 3. Chat Completion (`test_ollama_client_chat`)

**Purpose**: Verify client can send chat requests

**Steps**:

1. Start client with chat prompt
2. Wait for chat response
3. Validate:
    - Protocol is "Ollama"
    - Output shows "chat" or "message" or "response"
4. Stop client

**Runtime**: ~30 seconds

**LLM Calls**: 1

### 4. Custom Endpoint (`test_ollama_client_custom_endpoint`)

**Purpose**: Verify client works with explicit endpoint

**Steps**:

1. Start client with explicit "http://localhost:11434" endpoint
2. Wait for connection
3. Validate:
    - Shows "localhost:11434" or "11434" or "Ollama"
4. Stop client

**Runtime**: ~15 seconds

**LLM Calls**: 1

### 5. Error Handling (`test_ollama_client_error_handling`)

**Purpose**: Verify client handles connection errors

**Steps**:

1. Start client with invalid endpoint (localhost:99999)
2. Wait for error
3. Validate:
    - Shows "ERROR" or "error" or "failed" or "connect"
4. Stop client

**Runtime**: ~10 seconds

**LLM Calls**: 1

**Note**: This test doesn't require Ollama server

## Running Tests

**Feature-specific** (recommended):

```bash
./cargo-isolated.sh test --no-default-features --features ollama --test client::ollama::e2e_test
```

**All features** (slow):

```bash
./cargo-isolated.sh test --all-features --test client::ollama::e2e_test
```

## Requirements

### System Requirements

- **Ollama server**: Must be running on localhost:11434
- **Model**: `qwen2.5-coder:0.5b` must be available
- **Network**: Localhost only (no external connections)

### Setup

```bash
# 1. Start Ollama server
ollama serve

# 2. Pull required model
ollama pull qwen2.5-coder:0.5b

# 3. Run tests
./cargo-isolated.sh test --features ollama --test client::ollama::e2e_test
```

### Skip Behavior

`require_ollama()` skips when no Ollama answers `http://localhost:11434/api/tags` within 2s.
It is called **only** by the `#[ignore]`d `*_real` variants, which are not part of any run
unless asked for by name. The mocked tests need nothing running and must never call it: a
skip-when-missing gate is a silent pass, not evidence.

## Expected Output

```
test ollama_client_tests::test_ollama_client_list_models ... ok
test ollama_client_tests::test_ollama_client_generate ... ok
test ollama_client_tests::test_ollama_client_chat ... ok
test ollama_client_tests::test_ollama_client_custom_endpoint ... ok
test ollama_client_tests::test_ollama_client_error_handling ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Known Issues

### 1. Ollama Server Availability

**Issue**: Tests require Ollama server running

**Symptoms**: All tests panic with "Test skipped: Ollama server not available"

**Fix**: Start Ollama server: `ollama serve`

### 2. Model Not Found

**Issue**: Tests use `qwen2.5-coder:0.5b` which may not be available

**Symptoms**: Tests pass but generate requests fail

**Fix**: Pull model: `ollama pull qwen2.5-coder:0.5b`

### 3. Flaky Network Tests

**Issue**: Network tests can be timing-sensitive

**Symptoms**: Intermittent failures, timeouts

**Fix**:

- Increase timeout in test code
- Ensure Ollama server is responsive
- Check system load

### 4. Output Matching Sensitivity

**Issue**: Tests check for specific strings in output ("models", "response", etc.)

**Symptoms**: Tests fail if output format changes

**Fix**: Update test assertions to match new output format

## Performance

- **Total runtime**: ~100 seconds (5 tests)
- **Per test**: 10-30 seconds
- **Bottleneck**: Ollama API response time
- **Parallelization**: Safe (clients are independent)

## Test Efficiency

**Why < 10 LLM calls is efficient**:

- Each test validates one client capability
- LLM used only to parse initial prompt
- Actual API requests go to Ollama server
- Tests focus on protocol, not exhaustive scenarios

**Coverage**:

- ✅ Basic operations (list, generate, chat)
- ✅ Custom endpoint configuration
- ✅ Error handling
- ⚠️ No streaming tests (future)
- ⚠️ No concurrent client tests
- ⚠️ No embeddings tests

## Future Enhancements

1. **Streaming**: Test `stream: true` responses
2. **Embeddings**: Test embedding generation
3. **Multi-turn chat**: Test conversation state
4. **Model switching**: Test dynamic model selection
5. **Concurrent clients**: Test multiple clients
6. **Retry logic**: Test connection retry behavior
7. **Memory persistence**: Test conversation memory across requests

## Debugging

**Verbose output**:

```bash
./cargo-isolated.sh test --features ollama --test client::ollama::e2e_test -- --nocapture
```

**Check client logs**:

```bash
tail -f netget.log
```

**Manual testing**:

```bash
# Start NetGet client
./target/debug/netget "Connect to Ollama at http://localhost:11434 and list models"

# Watch output
# Client should connect and show models
```

**Test Ollama server manually**:

```bash
# Test if server is responding
curl http://localhost:11434/api/tags

# Test generate endpoint
curl -X POST http://localhost:11434/api/generate \
  -d '{"model":"qwen2.5-coder:0.5b","prompt":"Hello","stream":false}'
```

## CI/CD Considerations

**Environment setup**:

- CI needs Ollama server running
- Model must be pre-pulled
- Tests should skip if Ollama unavailable (already implemented)

**Docker example**:

```dockerfile
# Install Ollama in CI
RUN curl -fsSL https://ollama.com/install.sh | sh

# Start Ollama
RUN ollama serve &

# Pull model
RUN ollama pull qwen2.5-coder:0.5b

# Run tests
RUN cargo test --features ollama --test client::ollama::e2e_test
```

## Test Maintainability

**What to update when**:

1. **New Ollama endpoints**: Add new test case
2. **Output format changes**: Update string matching assertions
3. **Timeout too short**: Increase `Duration::from_secs()` values
4. **Model changed**: Update model name in tests
5. **Protocol changes**: Update expected output strings
