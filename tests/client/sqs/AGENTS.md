# SQS Client E2E Testing

## What actually runs

**One test runs by default: `command_channel_test.rs`**, which this file never mentioned. All
five tests in `e2e_test.rs` are `#[ignore]`d — four have no `.with_mock()` at all and need
`--use-ollama`, one needs LocalStack — so **the real LLM-call count for a default run is 0**,
not the 13 counted below, and none of the "covered scenarios" listed further down is covered
by anything that executes.

`command_channel_test.rs` proves the dashboard's `[ send ]` path end to end against a loopback
stub: an injected `send_message` becomes a real HTTP request whose body carries the message,
an unknown verb is `Rejected`, and `disconnect` drops the handle. It passes credentials
through the client's own startup parameters; it used to set four AWS environment variables
with `std::env::set_var`, which is process-global in a binary that runs at
`--test-threads=100`.

Keep the numbers below as *targets* for the ignored suite, not as a description of coverage.

## Test Strategy

Black-box E2E tests that verify SQS client functionality by spawning the actual NetGet binary and testing client
behavior with both a NetGet SQS server and optionally LocalStack.

## LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Actual**: 13 total LLM calls across 5 tests

### Call Breakdown

1. **test_sqs_client_connect_and_send**: 3 LLM calls
    - Server startup (1 call)
    - Client connection (1 call)
    - Message send operation (1 call)

2. **test_sqs_client_receive_messages**: 3 LLM calls
    - Server startup (1 call)
    - Client connection (1 call)
    - Receive messages operation (1 call)

3. **test_sqs_client_with_localstack**: 2 LLM calls (ignored by default)
    - Client connection (1 call)
    - Send and receive operations (1 call)

4. **test_sqs_client_invalid_queue**: 1 LLM call
    - Client connection attempt (1 call)

5. **test_sqs_client_get_attributes**: 2 LLM calls
    - Server startup (1 call)
    - Client connection + get attributes (1 call)

### Budget Justification

- **Server-based tests**: Use NetGet SQS server for controlled testing
- **LocalStack test**: Marked as `#[ignore]` by default (requires external service)
- **Efficient reuse**: Each test starts fresh server/client to ensure isolation
- **No scripting mode**: Tests use single instruction per instance

## Expected Runtime

**Development**: ~8-12 seconds (without LocalStack test)
**CI**: ~10-15 seconds (without LocalStack test)

### Runtime Breakdown

- Each test: ~2-3 seconds
- LLM latency: ~500-800ms per call (local Ollama)
- Server/client startup: ~500ms total
- Operation delays: 500-1000ms wait times

## Test Organization

### Test Files

```
tests/client/sqs/
├── e2e_test.rs         # Main E2E tests
├── CLAUDE.md           # This file
└── mod.rs              # Module declaration (to be created)
```

### Feature Gating

All tests are feature-gated with:

```rust
#[cfg(all(test, feature = "sqs"))]
```

This ensures tests only compile when the `sqs` feature is enabled.

## Test Coverage

### ✅ Covered Scenarios

1. **Basic Connection**: Client connects to SQS queue URL
2. **Send Message**: Client sends message with body and attributes
3. **Receive Messages**: Client polls and receives messages
4. **Get Attributes**: Client queries queue metadata
5. **Error Handling**: Invalid queue URL handling
6. **LocalStack Integration**: Real AWS SDK with LocalStack (optional)

### ❌ Not Covered (Future Work)

1. **Delete Message**: Message deletion after processing
2. **Purge Queue**: Clearing all messages
3. **Long Polling**: Wait time configuration
4. **Message Attributes**: Complex attribute types
5. **Visibility Timeout**: Message reappearance logic
6. **Batch Operations**: Send/receive multiple messages

## Running Tests

### Run All SQS Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features sqs --test client::sqs::e2e_test
```

### Run Single Test

```bash
./cargo-isolated.sh test --no-default-features --features sqs --test client::sqs::e2e_test test_sqs_client_connect_and_send
```

### Run with LocalStack Test

```bash
# First, start LocalStack
docker run -d -p 4566:4566 localstack/localstack

# Create test queue
aws --endpoint-url=http://localhost:4566 sqs create-queue --queue-name NetGetTestQueue

# Run test
./cargo-isolated.sh test --no-default-features --features sqs --test client::sqs::e2e_test test_sqs_client_with_localstack -- --include-ignored
```

## Known Issues

### 1. Credential Configuration

**Issue**: AWS SDK requires credentials even for LocalStack
**Workaround**: Set environment variables:

```bash
export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test
export AWS_DEFAULT_REGION=us-east-1
```

### 2. LocalStack Connectivity

**Issue**: LocalStack test assumes service is running
**Solution**: Test is marked `#[ignore]` by default

### 3. Timing Sensitivity

**Issue**: Operations may take longer with slow LLM
**Mitigation**: Tests use generous 1-2 second delays

### 4. No Message Persistence

**Issue**: NetGet SQS server doesn't persist messages
**Expected**: LLM maintains queue state in memory during test

## Test Maintenance

### When to Update Tests

- **New SQS actions added**: Add test for new operation
- **Error handling changes**: Update error test expectations
- **Performance improvements**: Reduce wait times if applicable
- **LocalStack version changes**: Update test configuration

### Adding New Tests

Follow the pattern:

1. Start server with specific instruction
2. Wait 500ms for server startup
3. Start client with queue URL and instruction
4. Wait 1000ms for client operation
5. Assert on client output or protocol
6. Cleanup server and client

### Performance Tuning

If tests become too slow:

- Reduce wait times (currently conservative)
- Use scripting mode for multi-step operations
- Batch operations in single test where appropriate
- Consider mocking AWS SDK calls (future)

## Comparison with Other Client Tests

| Client | Test Count | LLM Calls | Runtime | LocalStack |
|--------|------------|-----------|---------|------------|
| TCP    | 2          | 4         | ~4s     | No         |
| HTTP   | 3          | 6         | ~6s     | No         |
| Redis  | 2          | 4         | ~4s     | No         |
| SQS    | 5          | 13        | ~10s    | Optional   |

SQS tests have slightly higher LLM call count due to:

- Server-side logic complexity (queue state)
- More operation types (send, receive, delete, purge, attributes)
- Optional LocalStack integration test

## Future Improvements

1. **Scripting Mode**: Batch operations to reduce LLM calls
2. **Mock AWS SDK**: Unit tests without network calls
3. **Message Verification**: Assert on message content in server logs
4. **Error Scenarios**: Test throttling, permission errors, network failures
5. **Performance Tests**: Measure throughput with large message batches
6. **Integration with Real AWS**: Optional test against actual SQS (with cleanup)
