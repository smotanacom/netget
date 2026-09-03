# Socket File Protocol E2E Tests

## Test Strategy

The socket file E2E tests validate Unix domain socket functionality using real UnixStream clients to ensure the LLM
correctly handles IPC communication through filesystem socket files.

## Test Approach

**Black-box testing**: Tests use the NetGet binary as-is with LLM prompts. The LLM interprets prompts and generates
protocol responses. Tests validate with real Unix socket clients (tokio::net::UnixStream).

**Focus**: Core socket file functionality (echo, PING/PONG, line-based protocol) using simple prompts that minimize LLM
calls.

## LLM Call Budget

**Total LLM calls**: 3 tests × 1 call per test = **3 LLM calls**

### Test Breakdown

1. **test_socket_echo** (1 LLM call)
    - Prompt: Echo server on socket file
    - Action: Send "Hello, Socket!" → receive "ACK: Hello, Socket!"
    - Validation: Response contains "ACK" and echoes message

2. **test_socket_ping_pong** (1 LLM call)
    - Prompt: PING/PONG server on socket file
    - Action: Send "PING" → receive "PONG\n"
    - Validation: Response contains "PONG"

3. **test_socket_line_protocol** (1 LLM call)
    - Prompt: Line-based protocol on socket file
    - Action: Send "TEST COMMAND\n" → receive "OK: TEST COMMAND\n"
    - Validation: Response starts with "OK:" and contains command

**Rationale**: Each test uses one LLM call (per connection). Total: 3 calls, well under the 10-call budget.

## Expected Runtime

- **Per test**: ~5-15 seconds (socket creation + 1 LLM call + validation)
- **Total suite**: ~15-45 seconds
- **Model**: qwen3-coder:30b (default)

**Breakdown**:

- Server startup: ~1-2s
- Socket file creation: ~0.5s
- LLM response: ~3-10s (depends on model/load)
- Validation: <1s
- Cleanup: <1s

## Running Tests

### Prerequisites

```bash
# Build with socket_file feature
./cargo-isolated.sh build --release --no-default-features --features socket_file

# Ensure ./tmp directory exists (created automatically if needed)
```

### Execution

```bash
# Run socket file E2E tests
./cargo-isolated.sh test --no-default-features --features socket_file --test server::socket_file::test

# With verbose output
./cargo-isolated.sh test --no-default-features --features socket_file --test server::socket_file::test -- --nocapture
```

### Test Output

```
=== E2E Test: Socket File Echo Server ===
Server started with socket file
Connecting Unix socket client...
✓ Unix socket client connected
Sending: Hello, Socket!
Received: ACK: Hello, Socket!
✓ Socket file echo test passed
=== Test passed ===
```

## Known Issues

### 1. Platform Limitation

- **Issue**: Unix domain sockets are not supported on Windows
- **Impact**: Tests will fail on Windows
- **Workaround**: Only run on Linux/macOS/Unix systems

### 2. Socket File Cleanup

- **Issue**: If test crashes, socket file may remain in ./tmp
- **Impact**: Next test may fail if socket file already exists
- **Workaround**: Tests remove existing socket files before binding; manual cleanup with `rm ./tmp/netget-test-*.sock`

### 3. LLM Response Variability

- **Issue**: LLM may respond slightly differently (e.g., "ACK:" vs "Ack:")
- **Impact**: Assertion failures if case/format differs
- **Workaround**: Tests use contains() checks instead of exact matches where reasonable

### 4. Timeout on Slow Systems

- **Issue**: LLM may take longer than 10s timeout on slow systems or busy Ollama server
- **Impact**: Test failure with "Response timeout" error
- **Workaround**: Increase timeout or use faster model

### 5. Socket File Permissions

- **Issue**: ./tmp directory may not exist or be writable in some environments
- **Impact**: Socket file creation fails
- **Workaround**: Ensure ./tmp directory exists and is writable, or modify socket paths in tests

## Test Coverage

### Covered Scenarios

- ✓ Socket file creation and binding
- ✓ Client connection to socket file
- ✓ Data send/receive over Unix socket
- ✓ LLM-controlled echo responses
- ✓ LLM-controlled custom protocols (PING/PONG, line-based)
- ✓ Socket file cleanup

### Not Covered (Future Tests)

- ✗ Multiple concurrent connections on same socket file
- ✗ Socket file permissions and ownership
- ✗ Large data transfer (>8KB buffer)
- ✗ Binary protocol handling
- ✗ wait_for_more accumulation
- ✗ Connection timeout/idle handling
- ✗ Credential passing (SO_PEERCRED)

## Performance Notes

- **Faster than TCP**: No network stack overhead, direct IPC
- **LLM bottleneck**: Same as TCP - LLM response time dominates (3-10s)
- **Socket creation**: Very fast (<100ms) compared to TCP bind
- **Concurrency**: each test gets its own in-process mock LLM

## Comparison to TCP Tests

| Aspect        | TCP Tests             | Socket File Tests      |
|---------------|-----------------------|------------------------|
| **Client**    | tokio::net::TcpStream | tokio::net::UnixStream |
| **Address**   | IP:port               | Filesystem path        |
| **LLM Calls** | 4 tests, 1 call each  | 3 tests, 1 call each   |
| **Runtime**   | 20-60s                | 15-45s                 |
| **Platform**  | Cross-platform        | Unix/Linux only        |
| **Cleanup**   | Port released         | Socket file removal    |

## Future Enhancements

1. **Multi-connection test**: Validate concurrent clients on same socket file
2. **Binary protocol test**: Hex-encoded data send/receive
3. **Accumulation test**: wait_for_more for incomplete data
4. **Permission test**: Verify socket file permissions (chmod)
5. **Credential test**: SO_PEERCRED for client PID/UID/GID
