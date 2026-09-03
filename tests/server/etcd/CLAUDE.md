# etcd E2E Test Documentation

## Test Strategy

**Approach**: Black-box testing using real etcd client (`etcd-client` crate) against NetGet's etcd server
implementation.

**Philosophy**: Validate that real etcd clients can interact with NetGet's LLM-controlled etcd server for basic KV
operations.

## Test Structure

### Test Organization

- **Location**: `tests/server/etcd/e2e_test.rs`
- **Client Library**: `etcd-client` v0.15 (official Rust client)
- **Test Type**: End-to-end, non-interactive (prompt-driven)
- **Consolidation**: Single comprehensive test covering all KV operations

### Test File Structure

```
tests/server/etcd/
├── mod.rs                  # Module declaration
├── e2e_test.rs             # Main E2E test
└── CLAUDE.md               # This file
```

## LLM Call Budget

**Target**: < 10 total LLM calls for entire test suite

### Breakdown

1. **Server Startup**: 1 LLM call
    - Generate initial server logic from comprehensive prompt
    - Prompt covers all test scenarios (Put, Get, Delete, Txn)

2. **Test Operations**: 5-7 LLM calls
    - Put operation: 1 call
    - Get operation: 1 call
    - Get non-existent key: 1 call
    - Range query: 1 call
    - Delete operation: 1 call
    - Transaction (compare-and-swap): 1-2 calls

**Total Estimated**: 6-8 LLM calls

### Budget Optimization

- **No scripting**: etcd protobuf complexity makes scripting difficult in Phase 1
- **Consolidated server**: One server instance handles all test cases
- **Comprehensive prompt**: Single startup prompt covers all scenarios
- **Sequential tests**: Run operations in sequence against same server

## Scripting Mode

**Status**: Not enabled for etcd tests (Phase 1)

**Rationale**:

- Complex protobuf schema (nested messages, oneofs, etc.)
- Dynamic responses based on store state
- Transactions require conditional logic
- Future: Could enable scripting for simple Get/Put operations

## Client Library

### etcd-client v0.15

**Crate**: https://crates.io/crates/etcd-client
**Docs**: https://docs.rs/etcd-client/

**Features Used**:

- `Client::connect()` - Connect to etcd server
- `client.put()` - Store key-value pair
- `client.get()` - Retrieve value by key
- `client.delete()` - Delete key
- `client.txn()` - Execute transaction

**Example Usage**:

```rust
use etcd_client::Client;

let mut client = Client::connect(["localhost:2379"], None).await?;

// Put
client.put("foo", "bar", None).await?;

// Get
let resp = client.get("foo", None).await?;
assert_eq!(resp.kvs().first().unwrap().value_str()?, "bar");

// Delete
client.delete("foo", None).await?;
```

## Expected Runtime

**Total Duration**: ~30-50 seconds

**Breakdown**:

- Server startup: 10-15 seconds (LLM generates logic)
- Per-operation: 2-5 seconds each (LLM call per request)
- Client operations: < 1 second (network + protobuf encoding)
- Test overhead: ~5 seconds (setup, teardown, assertions)

## Failure Rate

**Expected**: 0-5% failure rate

**Common Failure Modes**:

- Ollama timeout (if overloaded)
- LLM returns malformed JSON
- Port already in use (rare with dynamic port allocation)
- Client connection timeout (if server startup slow)

**Mitigation**:

- Dynamic port allocation (port 0 in prompt)
- Reasonable timeouts (30s for startup, 10s per operation)
- Retry logic for transient failures (optional)

## Test Cases Covered

### 1. Basic Put/Get

**Operation**: Store and retrieve single key-value pair
**LLM Calls**: 2 (1 put + 1 get)
**Validation**:

- Put returns success
- Get returns correct value
- Metadata fields populated (revision, version)

### 2. Get Non-Existent Key

**Operation**: Query key that doesn't exist
**LLM Calls**: 1
**Validation**:

- Returns empty kvs array
- No error (etcd behavior)

### 3. Range Query (Prefix)

**Operation**: Query multiple keys with prefix
**LLM Calls**: 1
**Validation**:

- Returns all keys matching prefix
- Keys sorted correctly
- Count matches number of kvs

### 4. Delete Operation

**Operation**: Delete existing key
**LLM Calls**: 2 (1 delete + 1 get to verify)
**Validation**:

- Delete returns deleted count
- Subsequent get returns empty

### 5. Transaction (Compare-and-Swap)

**Operation**: Conditional update using transaction
**LLM Calls**: 1-2
**Validation**:

- Transaction succeeds when condition met
- Transaction fails when condition not met
- Responses match expected outcomes

## Known Issues

### Flaky Tests

**None currently**: Test suite is new and stable

**Potential Issues**:

- LLM timeout under heavy load
- Protobuf parsing errors (if LLM returns invalid data)

### Environmental Dependencies

- **Ollama**: Must be running with model available
- **Port 2379**: Should be free (or use dynamic allocation)
- **Network**: Localhost connectivity required

## Test Execution

### Build Release Binary

**Required** before running tests:

```bash
./cargo-isolated.sh build --release --all-features
```

### Run Tests

```bash
# Run etcd E2E tests only
./cargo-isolated.sh test --features etcd --test server::etcd::e2e_test

# Run with output
./cargo-isolated.sh test --features etcd --test server::etcd::e2e_test -- --nocapture

# Run specific test
./cargo-isolated.sh test --features etcd --test server::etcd::e2e_test test_etcd_kv_operations
```

### Debug Mode

```bash
# Enable trace logging
RUST_LOG=trace ./cargo-isolated.sh test --features etcd --test server::etcd::e2e_test -- --nocapture
```

## Example Test Prompt

The comprehensive prompt used in `e2e_test.rs`:

```
listen on port 0 via etcd

You are an etcd v3 key-value store server. Handle all KV operations:

1. When clients PUT a key-value pair, store it in memory with revision tracking
2. When clients GET a key, return the stored value if it exists
3. When clients DELETE a key, remove it and return deleted count
4. For RANGE queries, return all keys matching the prefix
5. For TRANSACTIONS, evaluate conditions and execute success or failure branch

Examples:
- PUT /config/database = "localhost:5432" → Success (revision 1)
- GET /config/database → "localhost:5432" (revision 1)
- DELETE /config/database → Deleted 1 key
- RANGE /config/ → Returns all keys starting with /config/

Track revisions:
- First PUT: create_revision=1, mod_revision=1, version=1
- Update: create_revision=1, mod_revision=2, version=2
- Each mutation increments global revision counter
```

## Success Criteria

### Must Pass

- ✅ Server starts successfully on dynamic port
- ✅ etcd-client can connect
- ✅ Put/Get/Delete operations work correctly
- ✅ Range queries return correct results
- ✅ Transactions evaluate conditions properly
- ✅ Total LLM calls < 10

### Nice to Have

- ⏳ Sub-30 second total runtime
- ⏳ Zero flaky failures
- ⏳ Proper error messages from LLM
- ⏳ Revision tracking works correctly

## Future Enhancements

### Phase 2

- **Watch tests**: Test real-time change notifications (requires streaming)
- **Lease tests**: Test TTL-based expiration
- **Auth tests**: Test authentication and authorization
- **Scripting**: Enable scripting mode for performance

### Additional Test Scenarios

- Large value storage (> 1MB)
- Concurrent client connections
- Bulk operations (many keys)
- Edge cases (empty keys, special characters)
- Error recovery (malformed requests)

## References

- [etcd-client Documentation](https://docs.rs/etcd-client/)
- [etcd v3 API Testing Guide](https://etcd.io/docs/v3.5/dev-guide/api_reference_v3/)
- [NetGet Test Guidelines](../../README.md)
