# SQS E2E Test Documentation

## Test Overview

The SQS E2E tests validate the AWS SQS-compatible message queue server implementation using the official `aws-sdk-sqs`
Rust client. Tests are designed to be efficient, minimizing LLM calls through comprehensive prompts and scripting mode.

**Protocol**: AWS SQS (Simple Queue Service) HTTP/JSON API
**Client Library**: `aws-sdk-sqs` v1.86 (official AWS SDK for Rust)
**Default Port**: 9324 (standard for local SQS)
**Target LLM Call Budget**: < 10 calls for entire test suite
**Expected Runtime**: ~25-35 seconds (depending on LLM response time)
**Stability**: Stable (no known flaky tests)

## Test Strategy

### Efficiency Through Consolidation

Following NetGet testing best practices:

1. **Reuse Server Instances**: Each test creates one comprehensive server with a detailed prompt covering multiple test
   scenarios
2. **Scripting Mode**: Enabled for repetitive operations (SendMessage) to reduce LLM calls from ~15 to ~5-8
3. **Comprehensive Prompts**: Single prompt describes all expected behaviors rather than separate servers for each test
   case
4. **Minimal Server Setups**: 3 server setups for entire suite (vs. 10+ if each test case had separate server)

### LLM Call Budget Breakdown

**Test 1: Basic Queue Operations** (~5 LLM calls)

- 1 server startup (with scripting for SendMessage)
- 1 CreateQueue request
- 3 SendMessage requests (handled by script = 0 additional LLM calls)
- 1 ReceiveMessage request
- 1 DeleteMessage request
- 1 GetQueueAttributes request

**Test 2: Message Visibility** (~3 LLM calls in mock mode)

- 1 server startup
- 1 CreateQueue request
- 1 SendMessage request
- 2 ReceiveMessage requests (combined in mock mode - visibility timeout is LLM-specific)
- 1 DeleteMessage request

Note: The visibility timeout behavior (message should not appear on second ReceiveMessage) can only be properly tested with real Ollama. In mock mode, both ReceiveMessage calls have identical parameters and use the same mock response.

**Test 3: Error Handling** (~2 LLM calls)

- 1 server startup
- 1 SendMessage to non-existent queue (error response)

**Total Budget**: 9-10 LLM calls (at budget target, less in mock mode)

**Note**: Actual call count may vary by ±1-2 depending on LLM prompt interpretation and scripting effectiveness.

## Scripting Mode Usage

### Enabled for SendMessage

SendMessage is a perfect candidate for scripting:

- **Predictable**: Always returns MessageId + MD5
- **Repetitive**: Test sends multiple messages
- **Simple state**: Just needs to track messages in queue

Script handles:

- Generating unique message IDs (`msg-<timestamp>-<random>`)
- Computing/returning MD5 checksums
- Storing message in queue structure
- Returning consistent JSON response

### Not Scripted

Operations requiring complex state inspection remain LLM-controlled:

- **ReceiveMessage**: Needs to check visibility timeouts, in-flight messages
- **DeleteMessage**: Needs to validate receipt handles
- **CreateQueue**: Initial queue setup and configuration

## Client Library Details

### aws-sdk-sqs Configuration

```rust
use aws_config::BehaviorVersion;
use aws_sdk_sqs::Client;

// Configure SDK to use local NetGet endpoint
let sdk_config = aws_config::defaults(BehaviorVersion::latest())
    .endpoint_url(format!("http://127.0.0.1:{}", port))
    .region(aws_config::Region::new("us-east-1"))
    .credentials_provider(aws_sdk_sqs::config::Credentials::new(
        "test", "test", None, None, "test",
    ))
    .load()
    .await;

let client = Client::new(&sdk_config);
```

### Key Operations Tested

1. **CreateQueue**
   ```rust
   client.create_queue()
       .queue_name("test-queue")
       .send()
       .await
   ```

2. **SendMessage**
   ```rust
   client.send_message()
       .queue_url(&queue_url)
       .message_body("Test message")
       .send()
       .await
   ```

3. **ReceiveMessage**
   ```rust
   client.receive_message()
       .queue_url(&queue_url)
       .max_number_of_messages(3)
       .send()
       .await
   ```

4. **DeleteMessage**
   ```rust
   client.delete_message()
       .queue_url(&queue_url)
       .receipt_handle(&receipt_handle)
       .send()
       .await
   ```

5. **GetQueueAttributes**
   ```rust
   client.get_queue_attributes()
       .queue_url(&queue_url)
       .attribute_names(aws_sdk_sqs::types::QueueAttributeName::All)
       .send()
       .await
   ```

## Test Cases

### 1. Basic Queue Operations (`test_sqs_basic_queue_operations`)

**Purpose**: Validate core SQS functionality in a single comprehensive test

**Operations Tested**:

- CreateQueue with queue name validation
- SendMessage (3 messages) with MD5 calculation
- ReceiveMessage with max message limit
- DeleteMessage with receipt handle
- GetQueueAttributes for queue metadata

**Assertions**:

- Queue URL contains queue name
- Each SendMessage returns MessageId and MD5
- ReceiveMessage returns messages (up to max)
- DeleteMessage succeeds with valid receipt handle
- GetQueueAttributes returns queue configuration

**Expected Behavior**:

- Messages persist across operations
- Messages appear in ReceiveMessage after SendMessage
- Messages can be deleted using receipt handle from ReceiveMessage

### 2. Message Visibility (`test_sqs_message_visibility`)

**Purpose**: Test visibility timeout and message lifecycle

**Operations Tested**:

- CreateQueue with visibility timeout configuration
- SendMessage to add message to queue
- ReceiveMessage marks message in-flight
- Subsequent ReceiveMessage should not return in-flight message
- DeleteMessage removes message permanently

**Assertions**:

- First ReceiveMessage returns 1 message
- Message has receipt handle
- Second ReceiveMessage respects visibility (may be flaky depending on LLM)
- DeleteMessage succeeds

**Expected Behavior**:

- Messages marked in-flight are not returned by ReceiveMessage
- Receipt handles are unique per receive operation
- Deleted messages never appear again

**Known Limitations**:

- Visibility timeout enforcement depends on LLM understanding (only testable with real Ollama)
- In mock mode, both ReceiveMessage calls are identical and cannot be differentiated
- The test passes in mock mode but doesn't validate visibility timeout behavior

### 3. Error Handling (`test_sqs_queue_not_found`)

**Purpose**: Validate proper error responses

**Operations Tested**:

- SendMessage to non-existent queue
- Verify 400 error response

**Assertions**:

- Request to non-existent queue returns error
- Error indicates queue does not exist

**Expected Behavior**:

- Operations on non-existent queues return QueueDoesNotExist error
- No automatic queue creation

## Expected Runtime

**Mocked (the default): the three tests together finish in under two seconds.** They used to
take about four seconds longer than that for no reason at all — eight fixed
`sleep(500ms)`/`sleep(300ms)` calls sat between AWS SDK operations that are each already
awaited, so the response, and therefore the mock call it provoked, had been recorded before
the sleep began. They are gone, and each test now ends with `wait_for_mocks(30)` before
`verify_mocks()`, which waits on the condition instead of guessing at a duration.

The numbers below are for `--use-ollama`, where a real model answers every operation.

**Total Suite**: ~25-35 seconds

- Test 1 (Basic Operations): ~12-15 seconds (1 setup + 5 operations)
- Test 2 (Visibility): ~8-10 seconds (1 setup + 3 operations)
- Test 3 (Error Handling): ~5-7 seconds (1 setup + 1 operation)

**Factors Affecting Runtime**:

- LLM response time (largest factor)
- Network latency to Ollama
- Script generation time
- Server startup time (~1-2 seconds)

## Failure Rate

**Expected**: < 5% failure rate

**Common Failure Modes**:

1. **LLM misunderstands queue state**: Message appears/disappears unexpectedly
2. **Visibility timeout confusion**: LLM returns in-flight messages
3. **Receipt handle mismatch**: LLM generates invalid handles
4. **MD5 calculation**: LLM returns inconsistent or missing MD5
5. **JSON formatting**: Malformed response bodies

**Mitigation**:

- Clear, explicit prompts with format examples
- Detailed queue state tracking instructions
- Visibility timeout explanation in prompt
- MD5 note: "you can use any deterministic value"

## Known Issues

### Test-Specific Issues

1. **Visibility Timeout Test Flakiness**:
    - LLM may not correctly track visibility timeout
    - Second ReceiveMessage may return message that should be in-flight
    - **Workaround**: Assertion commented or made lenient

2. **Message Ordering**:
    - Standard queues don't guarantee FIFO
    - Tests don't assume specific order

3. **MD5 Calculation**:
    - LLM may use simplified MD5 or placeholder value
    - Tests only verify MD5 field is present, not accuracy

### Infrastructure Issues

None currently identified. Tests are stable with proper Ollama setup.

## Running the Tests

### Prerequisites

1. **Build NetGet with all features**:
   ```bash
   ./cargo-isolated.sh build --no-default-features --features sqs
   ```

2. **Ollama must be running** with SQS-capable model:
   ```bash
   ollama serve
   ```

### Run SQS Tests Only

```bash
./cargo-isolated.sh test --features sqs --test server -- server::sqs
```

### Run Specific Test

```bash
./cargo-isolated.sh test --features sqs --test server -- server::sqs::e2e_test::test_sqs_basic_queue_operations
```

### Expected Output

```
running 3 tests
test test_sqs_basic_queue_operations ... ok
test test_sqs_message_visibility ... ok
test test_sqs_queue_not_found ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 28.45s
```

## Test Maintenance

### When to Update Tests

1. **Protocol Changes**: SQS API operations added/modified
2. **Action System Changes**: New actions or action format changes
3. **Scripting Changes**: Scripting mode implementation updates
4. **Bug Fixes**: Tests added to prevent regressions

### Updating LLM Call Budget

If adding new tests:

1. Document LLM calls for new test
2. Update budget breakdown
3. Ensure total remains < 15 calls
4. Consider consolidation opportunities

### Improving Test Efficiency

Current optimization opportunities:

1. **Batch Operations**: Add SendMessageBatch test (1 LLM call vs 3)
2. **Combined Error Tests**: Test multiple error scenarios in one server
3. **Attribute Variations**: Test different queue attributes in same test

## Comparison to DynamoDB Tests

### Similarities

- Both use AWS SDK clients
- Both minimize LLM calls through consolidation
- Both test HTTP/JSON API protocols
- Both use scripting for repetitive operations

### Differences

| Aspect         | DynamoDB                | SQS                               |
|----------------|-------------------------|-----------------------------------|
| **State**      | Stateless               | Stateful (queue persistence)      |
| **Operations** | CRUD (GetItem, PutItem) | Queue ops (Send, Receive, Delete) |
| **Complexity** | Simple key-value        | Message lifecycle with timeouts   |
| **Scripting**  | PutItem, GetItem        | SendMessage                       |
| **Flakiness**  | Very stable             | Visibility timeout can be flaky   |
| **LLM Budget** | ~8-10 calls             | ~10-11 calls                      |

**Key Difference**: SQS requires more complex state tracking (visibility timeouts, receipt handles), making it slightly
more prone to LLM confusion but still within acceptable failure rate.

## Debugging Failed Tests

### 1. Check Server Output

Server output shows LLM prompts and responses:

```bash
# Look for SQS event and action traces
[TRACE] SQS request: {"operation":"SendMessage",...}
[TRACE] SQS response: {"MessageId":"msg-123",...}
```

### 2. Verify Queue State

Check if LLM is tracking messages:

- Look for "remember messages" in prompt
- Verify LLM acknowledges message storage
- Check if receipt handles are being generated

### 3. Validate JSON Responses

Malformed JSON is a common issue:

- Missing MessageId or MD5
- Incorrect error format
- Missing receipt handles

### 4. Test Locally with Manual Client

Use `aws` CLI or boto3 to manually test:

```bash
# Start NetGet SQS server
netget "Start SQS server on port 9324..."

# Test with AWS CLI
aws sqs create-queue --queue-name test --endpoint-url http://localhost:9324
```

## Future Enhancements

### Potential New Tests

1. **Batch Operations**: SendMessageBatch, DeleteMessageBatch
2. **Long Polling**: WaitTimeSeconds parameter
3. **Message Attributes**: Custom message attributes
4. **Queue Management**: ListQueues, PurgeQueue, DeleteQueue
5. **FIFO Queues**: Message groups and deduplication (requires major changes)
6. **DLQ**: Dead Letter Queue configuration

### Scripting Improvements

1. **Script ReceiveMessage**: If LLM can provide queue state to script
2. **Script GetQueueAttributes**: Simple metadata response
3. **Pre-populate Queues**: Script creates initial test queues

### Performance Optimizations

1. **Parallel Tests**: Run independent tests concurrently
2. **Shared Server**: Multiple tests against same server instance
3. **Pre-built Binary**: Cache release build between test runs
