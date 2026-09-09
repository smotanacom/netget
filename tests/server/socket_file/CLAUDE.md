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

**Whole suite: about 3 seconds.** Every call is answered by an in-process `MockOllamaServer`, so
none of it waits on a model. The figures this section used to carry — "~5-15 seconds per test",
"LLM response: 3-10s", "Model: qwen3-coder:30b" — described a real-Ollama run that these tests
have not been for a long time. Last measured: 3 tests in 2.78s wall, alongside the other six IPC
suites.

The suite still spawns the NetGet **binary**, so it reads `target/debug/netget` and is subject to
the shared-`target/` contention CLAUDE.md warns about: a "protocol exists but is not compiled
into this build" failure during a parallel wave is another agent rebuilding, not a regression.

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

### 3. Fixed socket paths

- **Issue**: the three sockets are fixed paths under `./tmp/`, relative to the NetGet process cwd
- **Impact**: two concurrent runs of this suite collide; the three tests do not collide with each
  other because each has its own path
- **Workaround**: none; it has not been worth parameterising

### 4. Startup is waited for, not slept through

- **Issue**: `start_netget_server` returns when the harness has *parsed* the start line, which is
  before the protocol has created the socket node
- **Impact**: connecting immediately gets ENOENT
- **Resolution**: each test calls `wait_for_path(<socket>, 30)`, which polls for the node. The
  fixed 500ms sleep this used to be is exactly the shape that is "enough alone and not when a
  hundred run together"

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
- ✗ Socket file permissions and ownership — the server chmods the node to `0600` after bind, and
  nothing asserts it. The tests connect as the same user, so they would pass either way; only a
  second uid could prove the restriction, which a unit test cannot arrange
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
4. **Permission test**: assert `srw-------` on the node after start (a `metadata().permissions()`
   check is cheap and would at least catch the chmod being dropped, even if it cannot prove
   another user is refused)
5. **Credential test**: SO_PEERCRED for client PID/UID/GID
