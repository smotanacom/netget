# DynamoDB Client Testing

## Test Strategy

Black-box E2E testing using DynamoDB Local or LocalStack for local testing. Tests verify the AWS SDK integration and
DynamoDB operations work correctly.

## Prerequisites

Tests require a local DynamoDB instance running on localhost:8000:

### Option 1: DynamoDB Local (Recommended)

Download from: https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/DynamoDBLocal.html

```bash
# Run DynamoDB Local
java -Djava.library.path=./DynamoDBLocal_lib -jar DynamoDBLocal.jar -sharedDb -inMemory -port 8000
```

### Option 2: Docker

```bash
# Run DynamoDB Local via Docker
docker run -p 8000:8000 amazon/dynamodb-local -jar DynamoDBLocal.jar -inMemory -sharedDb
```

### Option 3: LocalStack

```bash
# Run LocalStack (includes DynamoDB)
docker run -p 4566:4566 localstack/localstack

# If using LocalStack, update endpoint_url in tests to http://localhost:4566
```

## Test Approach

### SDK-Based Tests (Current Implementation)

Tests use the `aws-sdk-dynamodb` crate directly to verify DynamoDB operations:

1. **Setup Phase**
    - Create test client pointing to localhost:8000
    - Create test table with schema
    - Wait for table to be active

2. **Test Phase**
    - Execute DynamoDB operation (PutItem, GetItem, etc.)
    - Verify response
    - Assert expected behavior

3. **Cleanup Phase**
    - Delete test table
    - Clean up resources

### Future: LLM-Controlled Tests

Once the NetGet client is fully integrated, tests will:

- Start NetGet with DynamoDB client configuration
- Use LLM instructions to trigger operations
- Verify LLM interprets responses correctly

## LLM Call Budget

**Current**: 0 LLM calls (SDK-only tests)

**Future** (with NetGet integration): < 5 LLM calls per suite

- Test 1: PutItem + GetItem (2 LLM calls)
- Test 2: Scan (1 LLM call)
- Test 3: UpdateItem (1 LLM call)
- Test 4: DeleteItem (1 LLM call)

## Expected Runtime

- **Current**: ~5 seconds (with DynamoDB Local running)
    - Table creation: ~500ms
    - Each operation: ~100-200ms
    - Table deletion: ~500ms

- **Future** (with NetGet): ~10 seconds
    - Additional LLM call overhead: ~1-2s per call

## Test Coverage

### Operations Tested

- ✅ PutItem - Insert items into table
- ✅ GetItem - Retrieve items by primary key
- ✅ Scan - Scan all items in table
- ✅ UpdateItem - Update existing items
- ✅ DeleteItem - Delete items

### Type Coverage

- ✅ String (S) - Tested in all operations
- ✅ Number (N) - Tested in PutItem, GetItem, UpdateItem
- ⏸ Binary (B) - Not tested yet
- ⏸ Boolean (BOOL) - Not tested yet
- ⏸ Map (M) - Not tested yet
- ⏸ List (L) - Not tested yet

### Error Cases

- ✅ Table creation (idempotent - handles existing table)
- ✅ Table deletion (idempotent - handles missing table)
- ⏸ Item not found (GetItem on non-existent item)
- ⏸ Invalid attribute types
- ⏸ Authentication errors

## Running Tests

```bash
# Make sure DynamoDB Local is running
java -Djava.library.path=./DynamoDBLocal_lib -jar DynamoDBLocal.jar -sharedDb -inMemory -port 8000

# Run DynamoDB client tests
./cargo-isolated.sh test --no-default-features --features dynamo --test client::dynamodb::e2e_test
```

## Known Issues

1. **Table Creation Timing**
    - DynamoDB Local may take up to 500ms for table to be active
    - Tests include sleep() to wait for table readiness
    - Production code should use waiter pattern

2. **Connection Refused**
    - If DynamoDB Local is not running, tests will fail with connection refused
    - Ensure DynamoDB Local is started before running tests

3. **Credentials Not Needed**
    - DynamoDB Local doesn't validate credentials
    - Any fake credentials work (e.g., "fakeAccessKeyId")
    - Production AWS requires valid credentials

## Test Data

All test tables use this schema:

- **Partition Key**: `id` (String)
- **Attributes**: `name` (String), `age` (Number)

Test data is cleaned up after each test (table deleted).

## Debugging

To see DynamoDB Local output:

```bash
# Run with verbose logging
java -Djava.library.path=./DynamoDBLocal_lib -jar DynamoDBLocal.jar -sharedDb -inMemory -port 8000 -dbPath ./tmp/dynamodb
```

To verify table creation:

```bash
# List tables (requires AWS CLI)
aws dynamodb list-tables --endpoint-url http://localhost:8000
```

## Future Improvements

1. **LLM Integration**
    - Convert SDK tests to NetGet client tests
    - Verify LLM can parse DynamoDB responses
    - Test LLM instruction interpretation

2. **Complex Types**
    - Test Map (M) and List (L) attributes
    - Test nested structures
    - Test empty values

3. **Error Handling**
    - Test invalid table names
    - Test invalid attribute types
    - Test conditional operations (ConditionExpression)

4. **Performance**
    - Test large batch operations (when implemented)
    - Test pagination (when implemented)
    - Measure latency

5. **Production Testing**
    - Add optional tests for real AWS DynamoDB
    - Skip by default, enable with environment variable
    - Use test tables with TTL for auto-cleanup
