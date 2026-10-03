# VNC Client E2E Testing

## Overview

This directory contains end-to-end tests for the VNC client implementation. Tests verify that the VNC client can connect
to VNC servers, send pointer/key events, and handle framebuffer updates.

## Test Strategy

### Approach: Black-Box Testing with NetGet Server

- **Server:** NetGet VNC server (custom RFB implementation)
- **Client:** NetGet VNC client
- **Verification:** Connection status, protocol detection, event sending
- **LLM Budget:** < 10 LLM calls per test suite (2-3 per test)

### Test Cases

#### 1. `test_vnc_client_connect_to_server`

**Purpose:** Verify basic VNC connection and handshake

**LLM Calls:** 2

- Server startup (1 call)
- Client connection (1 call)

**Flow:**

1. Start VNC server on random port
2. Start VNC client connecting to server
3. Verify client shows "connected" status
4. Verify RFB handshake completes

**Expected Runtime:** 3-4 seconds

---

#### 2. `test_vnc_client_pointer_event`

**Purpose:** Verify client can send pointer events (mouse)

**LLM Calls:** 2

- Server startup (1 call)
- Client with pointer action (1 call)

**Flow:**

1. Start VNC server that logs pointer events
2. Start VNC client with instruction to click at (100, 200)
3. Verify client protocol is VNC
4. Verify pointer event is sent

**Expected Runtime:** 3-4 seconds

---

#### 3. `test_vnc_client_key_event`

**Purpose:** Verify client can send keyboard events

**LLM Calls:** 2

- Server startup (1 call)
- Client with key action (1 call)

**Flow:**

1. Start VNC server that logs key events
2. Start VNC client with instruction to send 'A' key
3. Verify client connects
4. Verify key event is sent

**Expected Runtime:** 3-4 seconds

---

#### 4. `test_vnc_client_with_password` (IGNORED)

**Purpose:** Verify VNC authentication with password

**LLM Calls:** 2

- Server with password (1 call)
- Client with password (1 call)

**Status:** IGNORED - Current VNC auth implementation is simplified
**Reason:** DES encryption not fully implemented, may fail with strict servers

**Future:** Re-enable when full VNC authentication is implemented

---

## Running Tests

### Run all VNC client tests:

```bash
./cargo-isolated.sh test --no-default-features --features vnc --test client::vnc::e2e_test
```

### Run specific test:

```bash
./cargo-isolated.sh test --no-default-features --features vnc --test client::vnc::e2e_test test_vnc_client_connect_to_server
```

### Run with ignored tests:

```bash
./cargo-isolated.sh test --no-default-features --features vnc --test client::vnc::e2e_test -- --ignored
```

## LLM Call Budget

| Test                                | LLM Calls | Justification                       |
|-------------------------------------|-----------|-------------------------------------|
| `test_vnc_client_connect_to_server` | 2         | Server startup + client connection  |
| `test_vnc_client_pointer_event`     | 2         | Server startup + client action      |
| `test_vnc_client_key_event`         | 2         | Server startup + client action      |
| `test_vnc_client_with_password`     | 2         | Server with auth + client with auth |
| **TOTAL**                           | **8**     | **Well under 10 call limit**        |

## Expected Runtime

- **Per Test:** 3-4 seconds
- **Full Suite:** 12-16 seconds (4 tests, 3 ignored by default)
- **With Ignored:** 12-16 seconds (password test likely to fail)

## Known Issues

### VNC Authentication

- **Issue:** VNC authentication (Type 2) uses simplified implementation
- **Impact:** `test_vnc_client_with_password` may fail
- **Workaround:** Use security type 1 (None) for testing
- **Status:** Marked as `#[ignore]`

### Framebuffer Data

- **Issue:** Client doesn't parse actual pixel data from FramebufferUpdate
- **Impact:** LLM cannot analyze screen content
- **Workaround:** Tests focus on connection and event sending
- **Future:** Add pixel parsing for visual verification

### External VNC Servers

- **Issue:** Tests currently use NetGet VNC server only
- **Impact:** Limited real-world VNC server compatibility testing
- **Future:** Add tests with x11vnc, TigerVNC, RealVNC

## Test Infrastructure

Tests use the shared `helpers` module:

- `start_netget_server()` - Spawn server process
- `start_netget_client()` - Spawn client process
- `NetGetConfig` - Test configuration
- `E2EResult` - Test result type

## Debugging

### Enable verbose logging:

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features vnc --test client::vnc::e2e_test
```

### View netget.log:

```bash
tail -f netget.log
```

### Check process output:

Tests capture stdout/stderr from netget processes. Use `client.get_output()` in test assertions.

## Future Enhancements

1. **Real VNC Server Tests**
    - Test against x11vnc, TigerVNC
    - Verify compatibility with production VNC servers

2. **Framebuffer Validation**
    - Parse pixel data from updates
    - Verify screen content matches expectations

3. **Performance Tests**
    - Measure framebuffer update latency
    - Test with high-frequency pointer events

4. **Security Tests**
    - Test VNC authentication (Type 2) with real DES
    - Test TLS/VeNCrypt encrypted connections

5. **Multi-Client Tests**
    - Multiple clients connecting to same server
    - Verify shared desktop functionality

6. **Error Handling Tests**
    - Invalid credentials
    - Network interruptions
    - Protocol violations
