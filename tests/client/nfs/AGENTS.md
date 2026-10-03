# NFS Client E2E Test Strategy

## Test Approach

Black-box testing using the NetGet binary. Tests verify NFS client functionality by:

1. Starting NetGet NFS server with LLM-generated filesystem
2. Connecting NetGet NFS client to the server
3. Verifying client can perform file operations

## Test Server

**Target**: NetGet NFS server (LLM-controlled virtual filesystem)
**Port**: Dynamic (using `{AVAILABLE_PORT}` placeholder)
**Export**: `/data` (virtual export path)

**Advantages**:

- Complete control over filesystem structure
- Known file contents for verification
- Predictable error scenarios
- No external dependencies

## LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Current**: ~14 LLM calls total (4 tests × 3-4 calls avg)

### Per-Test Budget

| Test                               | LLM Calls | Breakdown                                                              |
|------------------------------------|-----------|------------------------------------------------------------------------|
| `test_nfs_client_mount_and_read`   | 4         | 1 server startup + 1 server file list + 1 client mount + 1 client read |
| `test_nfs_client_list_directory`   | 3         | 1 server startup + 1 client mount + 1 client list                      |
| `test_nfs_client_write_file`       | 4         | 1 server startup + 1 client mount + 1 client write + 1 client verify   |
| `test_nfs_client_create_directory` | 3         | 1 server startup + 1 client mount + 1 client mkdir                     |

**Total**: ~14 LLM calls (slightly over budget, but necessary for comprehensive testing)

## Expected Runtime

**Per test**: 3-5 seconds (including server startup and operation)
**Total suite**: 12-20 seconds

**Breakdown**:

- Server startup: 500-1000ms
- Client mount: 500-1000ms
- File operation: 1-2s (LLM processing)
- Verification: 100-500ms

## Test Scenarios

### 1. Mount and Read

**Purpose**: Verify client can mount NFS export and read file contents
**Server**: Provides file `readme.txt` with known content
**Client**: Mounts export, reads file, verifies content
**Verification**: Client output contains "mounted" or "NFS"

### 2. List Directory

**Purpose**: Verify client can list directory contents
**Server**: Provides directory with multiple files and subdirectories
**Client**: Lists root directory
**Verification**: Client protocol is "NFS"

### 3. Write File

**Purpose**: Verify client can write data to files
**Server**: Accepts write operations, logs them
**Client**: Creates file, writes content
**Verification**: Client output shows NFS activity

### 4. Create Directory

**Purpose**: Verify client can create directories
**Server**: Accepts mkdir operations, logs them
**Client**: Creates new directory
**Verification**: Client protocol is "NFS"

## Known Issues

### Current Limitations

1. **No Content Verification** - Tests don't verify exact file contents (would require additional LLM calls)
2. **Output Parsing** - Tests rely on fuzzy output matching ("mounted", "NFS") rather than structured verification
3. **Timing Sensitive** - Tests use fixed delays (1-3s) which may be insufficient for slow LLM responses
4. **No Error Testing** - Don't test error conditions (file not found, permission denied)

### Flaky Test Prevention

**Strategy**:

- Use generous timeouts (3s for operations)
- Fuzzy output matching (multiple acceptable patterns)
- Check protocol type rather than specific output
- Allow server startup time (1s)

**Potential Flakiness**:

- LLM may take longer than expected
- Server may not fully initialize before client connects
- File operations may complete in different order

### Future Improvements

1. **Structured Verification** - Parse structured output from client for exact verification
2. **Error Scenarios** - Test file not found, permission denied, invalid paths
3. **Concurrent Operations** - Test multiple clients accessing same export
4. **Large Files** - Test reading/writing large files (>4KB)
5. **Symlinks** - Test symlink creation and resolution (if implemented)

## Running Tests

### Single Test

```bash
./cargo-isolated.sh test --no-default-features --features nfs --test client::nfs::e2e_test::nfs_client_tests::test_nfs_client_mount_and_read
```

### Full Suite

```bash
./cargo-isolated.sh test --no-default-features --features nfs --test client::nfs::e2e_test
```

### With Ollama Lock

```bash
./cargo-isolated.sh test --no-default-features --features nfs --test client::nfs::e2e_test -- --test-threads=1
```

## Test Infrastructure

### Server Configuration

- Export path: `/data`
- Port: Dynamic (determined at runtime)
- Filesystem: LLM-generated (ephemeral)
- Files: Defined in test prompt

### Client Configuration

- Address format: `127.0.0.1:PORT:/data`
- Mount: Automatic on connection
- Operations: LLM-controlled based on instruction

### Cleanup

- Servers stopped after each test
- Clients disconnected gracefully
- No persistent state between tests

## LLM Call Optimization

### Current Strategy

- Minimal prompts (single sentence instructions)
- Direct operations (no multi-step workflows)
- Basic verification (output presence, not content)

### Potential Optimizations

1. **Scripting Mode** - Use scripting for file operations (eliminates per-operation LLM calls)
2. **Batched Operations** - Combine multiple operations in single prompt
3. **Reduced Verification** - Test fewer scenarios but more thoroughly

**Trade-off**: Optimization reduces test coverage and realism (LLM control is the feature)

## Test Data

### Server Filesystems

**Mount and Read**:

```
/data/
  readme.txt (content: "Hello from NFS server")
```

**List Directory**:

```
/data/
  file1.txt
  file2.txt
  docs/ (directory)
```

**Write File** (empty initially):

```
/data/
  (empty, accepts creates)
```

**Create Directory** (empty initially):

```
/data/
  (empty, accepts mkdir)
```

## Success Criteria

**All tests must**:

1. Complete without panics or crashes
2. Show client connection activity
3. Terminate cleanly (servers/clients stopped)
4. Run in < 5 seconds per test

**Pass criteria**:

- Mount test: Client shows "mounted" or "NFS" in output
- List test: Client protocol is "NFS"
- Write test: Client shows NFS activity
- Mkdir test: Client protocol is "NFS"

## Debugging Failed Tests

### Common Failures

1. **Connection Timeout**
    - Increase server startup delay (1s → 2s)
    - Check server actually started (port binding)
    - Verify address format (127.0.0.1:PORT:/data)

2. **Operation Not Executed**
    - Increase operation timeout (3s → 5s)
    - Check LLM understood instruction
    - Verify prompt clarity

3. **Output Missing**
    - Check for alternative output patterns
    - Verify client didn't crash early
    - Increase wait time before checking output

### Debug Commands

```bash
# Verbose test output
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features nfs --test client::nfs::e2e_test -- --nocapture

# Single test with logging
RUST_LOG=trace ./cargo-isolated.sh test --no-default-features --features nfs --test client::nfs::e2e_test::nfs_client_tests::test_nfs_client_mount_and_read -- --nocapture

# Keep processes alive for inspection
# (Modify test to add tokio::time::sleep(Duration::from_secs(60)) before cleanup)
```

## References

- [NFS Client Implementation](../../../src/client/nfs/CLAUDE.md)
- [NFS Server Implementation](../../../src/server/nfs/CLAUDE.md)
- [Test Infrastructure](../../README.md)
- [RFC 1813: NFSv3](https://tools.ietf.org/html/rfc1813)
