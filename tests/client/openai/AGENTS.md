# OpenAI Client E2E Tests

## Overview

E2E tests for the OpenAI client protocol verify that NetGet can correctly initialize the OpenAI client, construct proper API requests, and handle LLM-controlled actions through mocked responses.

**IMPORTANT**: These tests use mocks by default and do NOT require OpenAI API keys or network access.

## Test Strategy

### Black-Box Testing with Mocks

Tests spawn the NetGet binary as a subprocess and verify behavior by:

1. Mocking LLM responses to control client actions
2. Parsing output for expected protocol names and events
3. Validating client state transitions (connecting → connected)
4. Verifying mock expectations were met (correct number of LLM calls)

### Mock-Based Testing

All tests use the `.with_mock()` builder pattern to:
- Define expected LLM instruction patterns
- Specify action responses (open_client, send_chat_completion, etc.)
- Set expected call counts with `.expect_calls(N)`
- Verify all mocks were called with `.verify_mocks().await?`

This approach ensures:
- ✅ No external dependencies (OpenAI API)
- ✅ No API costs
- ✅ Fast test execution (<500ms per test)
- ✅ Deterministic behavior
- ✅ No environment variable requirements

### Test Coverage

**Covered:**

- ✅ Basic chat completion request (with mocks)
- ✅ Model selection (gpt-3.5-turbo, gpt-4)
- ✅ Embedding generation (with mocks)
- ✅ Custom parameters (organization, temperature)
- ✅ LLM-controlled request construction

**Not Covered (Future):**

- ❌ Function calling
- ❌ Streaming responses
- ❌ Multi-turn conversations
- ❌ Actual API integration (optional real-Ollama tests)
- ❌ Rate limiting
- ❌ Token usage tracking

## LLM Call Budget

**Total LLM Calls: 4** (well under 10 limit)

### Breakdown by Test

1. **test_openai_client_chat_completion_with_mocks**: 1 LLM call
    - Client startup (mock instruction → open_client action)

2. **test_openai_client_with_model_selection_with_mocks**: 1 LLM call
    - Client startup (mock instruction → open_client action with gpt-4)

3. **test_openai_client_embeddings_with_mocks**: 1 LLM call
    - Client startup (mock instruction → open_client action for embeddings)

4. **test_openai_client_custom_parameters_with_mocks**: 1 LLM call
    - Client startup (mock instruction → open_client action with custom params)

### Budget Justification

- Each test requires 1 mocked LLM call (startup only)
- No actual OpenAI API calls (requires real API key and network access)
- Tests verify client initialization and parameter handling
- Connection events not tested since OpenAI requires real API connectivity

## Expected Runtime

**Per-test runtime:** ~500ms per test (no API calls)
**Total suite:** ~2 seconds for all 4 tests

**Factors affecting runtime:**
- NetGet binary startup time (~200ms)
- Mock verification overhead (~100ms)
- No network latency (mocks only)

## Test Requirements

### Environment Variables

**None required.** Tests use mocks and do not call external APIs.

### Network Access

**None required.** Tests do not make real OpenAI API calls.

### API Cost

**$0.00** - No API calls are made (mocked responses only)

## Running Tests

### Local Development

```bash
# Run OpenAI client tests (no API key needed)
./cargo-isolated.sh test --no-default-features --features openai --test client::openai::e2e_test

# Run all client tests (includes OpenAI)
./cargo-isolated.sh test --features openai
```

### CI/CD Integration

Tests run in CI without any special configuration:

```yaml
# GitHub Actions example
- name: Run OpenAI tests
  run: ./cargo-isolated.sh test --features openai
  # No secrets or environment variables needed
```

## Test Implementation Details

### Test 1: Basic Chat Completion (Mocked)

**Purpose:** Verify OpenAI client initialization for chat completion

**Prompt:**
```
"Connect to OpenAI API with key 'sk-test-key' and send a chat completion: 'Say hello in exactly 3 words.'"
```

**Mocked Responses:**
1. Instruction → `open_client` action with OpenAI protocol and startup_params

**Expected Behavior:**
- Client initializes with OpenAI protocol
- Startup parameters include API key and default model
- Mock expectations verified (1 call)

**Assertions:**
- Output contains "OpenAI" or "openai"
- Mock call count matches expectation

### Test 2: Model Selection (Mocked)

**Purpose:** Verify model parameter in startup params

**Prompt:**
```
"Connect to OpenAI with key 'sk-test-key' using model gpt-4 and ask: 'What is 2+2?'"
```

**Mocked Responses:**
1. Instruction → `open_client` action with `default_model: "gpt-4"`

**Expected Behavior:**
- Client initializes with specified gpt-4 model
- Startup parameters include custom model

**Assertions:**
- Client protocol is "OpenAI"
- Mock expectations verified

### Test 3: Embeddings (Mocked)

**Purpose:** Verify OpenAI client initialization for embeddings

**Prompt:**
```
"Connect to OpenAI with key 'sk-test-key' and generate embeddings for the text: 'The quick brown fox'"
```

**Mocked Responses:**
1. Instruction → `open_client` action with instruction about embeddings

**Expected Behavior:**
- Client initializes with OpenAI protocol
- Instruction preserved for future embedding request

**Assertions:**
- Output contains "OpenAI"
- Mock expectations verified

### Test 4: Custom Parameters (Mocked)

**Purpose:** Verify custom parameter handling (organization)

**Prompt:**
```
"Connect to OpenAI with key 'sk-test-key' and organization 'org-test' and ask: 'Hello'"
```

**Mocked Responses:**
1. Instruction → `open_client` action with `organization: "org-test"`

**Expected Behavior:**
- Client initializes with custom organization parameter
- Startup params include all custom fields

**Assertions:**
- Output contains "OpenAI"
- Mock expectations verified

## Known Issues

### None

Tests are deterministic and use mocks, so there are no flaky tests, network issues, or API rate limits.

## Debugging Failed Tests

### Common Failure Modes

1. **Mock expectation not met**
    - Cause: LLM was not called expected number of times
    - Fix: Check mock configuration and instruction patterns

2. **Incorrect action construction**
    - Cause: Mocked actions have wrong format
    - Fix: Verify action JSON matches protocol expectations

3. **Test timeout**
    - Cause: NetGet binary hung or crashed
    - Fix: Check NetGet logs for errors

### Viewing Test Output

```bash
# Run with verbose output
./cargo-isolated.sh test --no-default-features --features openai --test client::openai::e2e_test -- --nocapture

# Check NetGet logs
tail -f netget.log
```

## Future Enhancements

1. **Streaming tests**: Verify streaming response handling (requires mock streaming support)
2. **Function calling**: Test tool usage and function calls
3. **Multi-turn conversations**: Verify conversation history management
4. **Real API tests**: Optional tests with real OpenAI API (requires API key, separate test suite)
5. **Vision API**: Test image inputs (GPT-4 Vision)
6. **Custom endpoints**: Test OpenAI-compatible APIs (e.g., Azure OpenAI)

## References

- **OpenAI API Docs**: https://platform.openai.com/docs/api-reference
- **Test Helpers**: `tests/helpers/client.rs`
- **Mock System**: `tests/helpers/mock.rs`
