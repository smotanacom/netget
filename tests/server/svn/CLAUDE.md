# SVN E2E Test Documentation

## Test Strategy

Black-box testing of SVN protocol implementation using real TCP clients. Tests verify the LLM's ability to understand
SVN protocol commands and generate appropriate responses.

**Status**: Experimental Protocol Testing
**Client**: Manual TCP connection with SVN protocol parsing
**Model**: the in-process mock (`tests/helpers/mock_ollama.rs`). No Ollama, no
`--ignored`; the whole directory compiles and runs under
`--no-default-features --features svn`.

Three files, not one — this document used to describe only the first:

| File | What it covers | LLM calls |
|---|---|---|
| `e2e_test.rs` | the five mocked command cases below | 13 mocked |
| `llm_failure_test.rs` | a dead backend produces a well-formed `failure` tuple carrying only a `WireFailure` category, then EOF | 0 (backend unreachable by design) |
| `peer_inject_test.rs` | `send_to_peer` writes a success tuple to a raw socket, the counters move, `close_connection` sends EOF | 0 |

## Test Coverage

### 1. Protocol Greeting (`test_svn_greeting`)

**Validates**: Server sends proper SVN protocol greeting on connection
**Flow**:

1. Start SVN server with greeting instructions
2. Connect via TCP
3. Read greeting message
4. Verify format contains "success" and version info

**Expected**: `( success ( 2 2 ( ANONYMOUS ) ( edit-pipeline svndiff1 ) ) )`

**LLM Calls**: 1 (server startup with greeting instruction)

### 2. Get Latest Revision (`test_svn_get_latest_rev`)

**Validates**: LLM responds to `get-latest-rev` command with revision number
**Flow**:

1. Start SVN server with revision instruction
2. Connect and read greeting
3. Send `( get-latest-rev )` command
4. Verify response contains success and revision number

**Expected**: `( success ( 42 ) )`

**LLM Calls**: 2 (startup + 1 command)

### 3. Directory Listing (`test_svn_get_dir`)

**Validates**: LLM responds to `get-dir` command with directory listing
**Flow**:

1. Start SVN server with directory listing instruction
2. Connect and read greeting
3. Send `( get-dir )` command
4. Verify response contains directory entries (trunk, branches, tags)

**Expected**: `( success ( 0 ( ) ( ( 5:trunk dir 0 false 1 ( ) ( ) ) … ) ) )` —
**counted strings, not quoted ones.** svn has no quoting mechanism: a string is
its byte length, a colon, then the raw bytes. This document showed `"trunk"`,
which is five characters and two stray quote marks that no svn parser accepts,
and which the server stopped emitting when `send_svn_list` was repaired.

**LLM Calls**: 2 (startup + 1 command)

### 4. Error Response (`test_svn_error_response`)

**Validates**: LLM can generate SVN error responses
**Flow**:

1. Start SVN server with error response instruction
2. Connect and read greeting
3. Send command
4. Verify response contains "failure" and error message

**Expected**: `( failure ( ( 210005 14:Path not found 0: 0 ) ) )` — the tuple is
`( apr-err message:string file:string line:number )`, with counted strings.

**LLM Calls**: 2 (startup + 1 command)

### 5. Connection Statistics (`test_svn_connection_stats`)

**Validates**: Server properly tracks connection metrics
**Flow**:

1. Start SVN server
2. Send command
3. Wait for stats update
4. Verify connection tracking via AppState

**LLM Calls**: 2 (startup + 1 command)

## LLM Call Budget

**Total LLM Calls**: 9 calls across 5 tests

- 5 server startups (1 per test)
- 4 command responses (tests 2-5)

**Budget Compliance**: ✓ Well under 10 calls per test
**Optimization**: Tests reuse single connection where possible

## Performance Characteristics

### Runtime

- **Per Test**: 12-15 seconds
    - Server startup: 2-4 seconds (LLM generates greeting script)
    - Command processing: 2-5 seconds per command (LLM call)
    - Teardown: < 1 second
- **Full Suite**: ~60-75 seconds (5 tests)

### LLM Performance

- **Model**: qwen2.5-coder:0.5b (fast, good for protocol work)
- **Temperature**: 0.7 (balanced)
- **Token Usage**: Low (protocol commands are short)
- **Accuracy**: High (protocol format is deterministic)

## Known Test Limitations

### 1. Simplified Protocol Testing

- Tests only verify basic command-response patterns
- No full SVN client library (manual TCP/protocol)
- No binary protocol features (svndiff, delta encoding)
- No authentication beyond ANONYMOUS

### 2. Response Validation

- Tests check for keywords ("success", "failure", "42")
- Full S-expression parsing would be more robust
- Lenient validation accounts for LLM variations

### 3. No Repository Operations

- No actual checkout/commit/update workflows
- No multi-command sequences (would increase LLM calls)
- Focus on protocol mechanics, not repository logic

### 4. Single Connection Model

- Tests use one connection per operation
- No connection pooling or reuse testing
- No concurrent connection testing

## Test Infrastructure

### Setup

```bash
# --test names a target, so `--test server` then filter by module path.
./cargo-isolated.sh test --no-default-features --features svn \
    --test server -- server::svn --test-threads=100
```

### Requirements

Nothing external. The mock model runs in-process, every socket binds
`127.0.0.1:0`, and no test is `#[ignore]`d. (This section used to require Ollama,
a downloaded model and `OLLAMA_LOCK_PATH`; the tests have been mocked for a long
time, and `--ollama-lock` is a documented no-op — see the root `CLAUDE.md`.)

### Debugging

Enable trace logging to see full SVN protocol messages:

```bash
RUST_LOG=netget=trace ./cargo-isolated.sh test --no-default-features --features svn \
    --test server -- server::svn --nocapture
```

## Future Improvements

### Test Coverage Expansion

- Multi-command sessions (get-dir → get-file)
- Authentication mechanism testing
- Binary protocol features (if implemented)
- Error handling for malformed commands
- Repository structure validation

### Performance Optimization

- Scripting mode for instant responses (0 LLM calls per command)
- Batch command testing
- Connection reuse patterns
- Stress testing with concurrent connections

### Client Improvements

- Full S-expression parser
- Binary protocol support
- Proper SVN client library (if available)
- More comprehensive response validation

## References

- [SVN Protocol Specification](https://svn.apache.org/repos/asf/subversion/trunk/subversion/libsvn_ra_svn/protocol)
- [NetGet Test Infrastructure](../../TEST_INFRASTRUCTURE_FIXES.md)
- [NetGet Test Status](../../TEST_STATUS_REPORT.md)
- [Whois E2E Tests](../whois/e2e_test.rs) (similar pattern)
