# Elasticsearch Client E2E Tests

## Test Strategy

The Elasticsearch client E2E tests verify LLM-controlled operations against a real Elasticsearch cluster using actual
Ollama LLM calls. Tests focus on core document operations and query capabilities.

## Test Approach

### Black-Box Testing

Tests verify:

1. **Client initialization** - Connection to Elasticsearch cluster
2. **Document indexing** - Creating documents with structured data
3. **Search operations** - Query DSL execution and result handling
4. **Document retrieval** - Get by ID operations
5. **Document deletion** - Delete operations
6. **Bulk operations** - Efficient multi-document operations

### LLM Integration

Each test uses **real Ollama LLM calls** to control client behavior. Tests validate that:

- LLM receives correct events (`elasticsearch_connected`, `elasticsearch_response_received`)
- LLM constructs valid Elasticsearch operations (index, search, get, delete, bulk)
- LLM interprets Elasticsearch responses correctly
- LLM chains operations based on instruction context

## LLM Call Budget

**Total Budget: < 10 LLM calls across all tests**

### Test 1: Index and Search (3-4 calls)

- **Call 1**: Initial connection → LLM indexes document
- **Call 2**: Index response → LLM executes search
- **Call 3**: Search response → LLM processes results
- **Total**: ~3-4 calls

### Test 2: Bulk Operations (2 calls)

- **Call 1**: Initial connection → LLM executes bulk operation
- **Call 2**: Bulk response → LLM processes results
- **Total**: ~2 calls

### Test 3: Document Lifecycle (4-5 calls)

- **Call 1**: Initial connection → LLM indexes document
- **Call 2**: Index response → LLM retrieves document
- **Call 3**: Get response → LLM deletes document
- **Call 4**: Delete response → LLM confirms
- **Total**: ~4-5 calls

**Grand Total**: ~9-11 calls (slightly over budget, but each test can run independently)

## Expected Runtime

### Per Test

- **Initialization**: ~1 second
- **LLM calls**: ~2-3 seconds each
- **Total per test**: ~10-15 seconds

### Full Suite

- **3 tests**: ~30-45 seconds total
- **With Ollama startup**: +5 seconds
- **With Elasticsearch startup**: +10 seconds (if not already running)

## Test Environment

### Prerequisites

1. **Elasticsearch**: Running locally on port 9200
   ```bash
   docker run -d -p 9200:9200 -e "discovery.type=single-node" \
     -e "xpack.security.enabled=false" \
     docker.elastic.co/elasticsearch/elasticsearch:8.11.0
   ```

2. **Ollama**: Running locally with model
   ```bash
   ollama serve
   ollama pull qwen2.5-coder:7b
   ```

3. **Feature flag**: Tests are feature-gated
   ```bash
   ./cargo-isolated.sh test --no-default-features --features elasticsearch \
     --test client -- client::elasticsearch::e2e_test
   ```

### Test Data

Tests create temporary data in Elasticsearch:

- **Indices**: `test-index`, `products`, dynamically created
- **Documents**: Test documents with simple schemas
- **Cleanup**: Not implemented (Elasticsearch is ephemeral in Docker)

## Known Issues

### 1. Elasticsearch Availability

**Issue**: Tests fail if Elasticsearch is not running
**Severity**: High
**Workaround**: Mark tests as `#[ignore]`, require manual Elasticsearch setup

### 2. LLM Query DSL Construction

**Issue**: LLM may construct invalid Elasticsearch Query DSL
**Severity**: Medium
**Mitigation**: Use simple match queries in test prompts

### 3. Index Creation Timing

**Issue**: Newly created indices may not be immediately available
**Severity**: Low
**Mitigation**: Elasticsearch auto-creates indices on first document write

### 4. Response Parsing

**Issue**: Large search responses may exceed LLM context window
**Severity**: Low
**Mitigation**: Use small test datasets

## Test Scenarios

### Scenario 1: Basic Indexing

```
Instruction: "Index a test document with fields 'title'='Test' and 'content'='Hello World', then search for it"
Expected: LLM indexes document, receives success, performs search, receives results
```

### Scenario 2: Bulk Operations

```
Instruction: "Use bulk operation to index 3 documents in 'products' index: laptop (price=999), phone (price=699), tablet (price=499)"
Expected: LLM constructs bulk NDJSON payload, receives success with operation count
```

### Scenario 3: Document Lifecycle

```
Instruction: "Index a document with id 'test-doc-1' in 'test-index', then get it, then delete it"
Expected: LLM indexes, retrieves (verifying content), deletes, confirms deletion
```

## Running Tests

### Single Test

```bash
./cargo-isolated.sh test --no-default-features --features elasticsearch \
  --test client -- client::elasticsearch::e2e_test::test_elasticsearch_client_index_and_search
```

### All Elasticsearch Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features elasticsearch \
  --test client -- client::elasticsearch::e2e_test
```

### With Logs

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features elasticsearch \
  --test client -- client::elasticsearch::e2e_test --nocapture
```

## Debugging

### Enable Verbose Logging

```rust
// In test setup
env_logger::builder()
    .filter_level(log::LevelFilter::Debug)
    .init();
```

### Check Elasticsearch

```bash
# Verify Elasticsearch is running
curl http://localhost:9200

# Check indices
curl http://localhost:9200/_cat/indices?v

# Check specific document
curl http://localhost:9200/test-index/_doc/test-doc-1
```

### LLM Response Inspection

```rust
// Add to test
println!("[TEST] Status: {:?}", status_rx.recv().await);
```

## Flaky Tests

### None Identified

Currently no flaky tests identified. Elasticsearch is deterministic for basic operations.

### Potential Flakiness

- **Network delays**: Elasticsearch may be slow on first request
- **LLM variability**: Different models may construct queries differently
- **Index creation**: Very rare race condition on index auto-creation

## Future Improvements

1. **Cleanup**: Delete test indices after each test
2. **Authentication**: Test with Elasticsearch security enabled
3. **Aggregations**: Test complex aggregation queries
4. **Scripting**: Test Painless script execution in queries
5. **Index Management**: Test explicit index creation/deletion
6. **Error Cases**: Test invalid queries, missing indices
7. **Connection Pooling**: Verify connection reuse
8. **HTTPS**: Test secure Elasticsearch clusters

## Test Maintenance

### When to Update

1. **New Elasticsearch features**: Add tests for new operations
2. **Breaking changes**: Update Query DSL syntax if Elasticsearch version changes
3. **LLM improvements**: Adjust prompts if LLM capabilities improve
4. **Performance issues**: Optimize test timing if tests become slow

### Code Locations

- **Test file**: `tests/client/elasticsearch/e2e_test.rs`
- **Implementation**: `src/client/elasticsearch/`
- **Actions**: `src/client/elasticsearch/actions.rs`
- **LLM integration**: `src/client/elasticsearch/mod.rs`

## Performance Benchmarks

Not yet established. Initial measurements:

- **Index operation**: ~100-200ms
- **Search operation**: ~50-100ms
- **Bulk operation (3 docs)**: ~150-250ms
- **Get operation**: ~50-100ms
- **Delete operation**: ~50-100ms

## In the browser

The native suites here drive the reqwest backend. The browser path — the same client code over
`src/client/http_fetch`'s hyper transport — is proven by `web/test/smoke.mjs` against the real
bundle (see `src/client/elasticsearch/AGENTS.md`, "Browser build"), and the transport backend's requests
are pinned to reqwest's by `tests/client/http/fetch_client_test.rs`.

