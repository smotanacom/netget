# SOCKS5 Client E2E Tests

## Overview

This test suite validates the SOCKS5 client implementation by connecting through a simple SOCKS5 proxy to an echo server
and verifying data transmission.

## Test Strategy

**Approach:** Minimal LLM integration tests with custom SOCKS5 proxy implementation

**Test Infrastructure:**

1. **Echo Server** - Simple TCP echo server that reflects received data back
2. **SOCKS5 Proxy** - Custom minimal SOCKS5 v5 proxy server (no authentication)
3. **NetGet Client** - SOCKS5 client connecting through proxy to echo server

**Why Custom Proxy?**

- No external dependencies (dante, ss5) required
- Full control over proxy behavior for testing edge cases
- Supports both no-auth and username/password authentication modes
- Lightweight and fast for CI/CD environments

## `action_test.rs` — fourteen `#[tokio::test]`, **0 LLM calls**, no proxy

The model-facing surface, asserted with nothing running: `execute_action` is pure, so every
assertion is about bytes. This is the only part of the client covered by tests that run
everywhere — the three `e2e_test.rs` tests are `#[ignore]`d and point at a real Ollama.

`send_socks5_data` used to declare one field, `data_hex`, and `hex::decode` it
unconditionally, so the static-mode startup example showed
`GET / HTTP/1.1\r\nHost: example.com\r\n\r\n` as 74 hex characters. It now takes `data` plus an
explicit `encoding` (`"utf8"` by default, or `"hex"`), the `send_tcp_data` shape.

- **`text_that_looks_like_hex_is_sent_as_its_characters`** is the assertion the original defect
  would have failed: `"48656c6c6f"`, `"deadbeef"` and `"0123456789abcdef"` go out as their
  literal characters under `encoding: "utf8"` **and** with `encoding` omitted, while
  `the_same_string_with_encoding_hex_is_the_decoded_bytes` shows those same strings meaning
  something entirely different when the sender says so. Nothing is sniffed — that is the
  whole point of the field.
- `send_socks5_data_declares_data_and_encoding_and_not_data_hex` — the declared parameter list
  is exactly `["data", "encoding"]`, and neither the description nor the parameters mention
  the deprecated field.
- `the_legacy_data_hex_field_still_sends_its_bytes` and
  `supplying_both_spellings_is_refused_by_name` are the backward-compatibility contract:
  `data_hex` alone still works so an existing static handler does not break, both together is
  **refused** naming both fields and saying neither takes precedence.
- `received_bytes_are_readable_when_they_are_text_and_hex_when_they_are_not` and
  `an_echo_built_from_the_event_reproduces_the_received_bytes` cover the inbound half:
  `socks5_data_received` carried `data_hex` only, so the most likely payload on this tunnel —
  an HTTP response — arrived as hex. It now carries `data` + `encoding` + `data_length`, and
  copying those two fields into `send_socks5_data` reproduces the bytes.
- `declared_event_parameters_match_the_payload` also pins the `get_event_types()` repair: the
  two events were rebuilt there with a `{"type": "placeholder"}` example and no parameters,
  and are now clones of the statics `mod.rs` raises.
- `every_declared_example_is_accepted_by_its_own_executor` and
  `no_example_hands_the_model_a_hex_blob` are the local forms of `executable_examples_test`
  and `example_hex_drift_test`, asserted against the values a running build produces rather
  than against the source text.

## LLM Call Budget

**Target:** < 10 LLM calls total

**Test Breakdown:**

1. `test_socks5_client_no_auth_basic` - 2-3 LLM calls
    - Connect event → LLM sends initial data
    - Data received event → LLM processes echo response

2. `test_socks5_client_connection_failure` - 0 LLM calls
    - Connection fails before LLM is invoked
    - Pure error handling test

3. `test_socks5_client_missing_target_addr` - 0 LLM calls
    - Parameter validation error before connection
    - Pure error handling test

**Total LLM Calls:** 2-3 (well under budget)

## Test Scenarios

### Test 1: Basic Connection (No Authentication)

**Test:** `test_socks5_client_no_auth_basic`

**Setup:**

- Echo server on random port (e.g., 54321)
- SOCKS5 proxy on random port (e.g., 54322)
- NetGet client instruction: "Connect through SOCKS5 proxy to echo server and send 'HELLO'"

**Flow:**

1. Client connects to SOCKS5 proxy
2. Proxy negotiates with target echo server
3. LLM receives `socks5_connected` event
4. LLM sends data through tunnel (`send_socks5_data`)
5. Echo server reflects data back
6. LLM receives `socks5_data_received` event
7. LLM processes echoed data

**Expected Result:**

- Connection succeeds
- Data flows through tunnel
- Client status: `Connected` or `Disconnected` (after completing task)

**LLM Calls:** 2-3

### Test 2: Connection Failure

**Test:** `test_socks5_client_connection_failure`

**Setup:**

- No SOCKS5 proxy running
- Client attempts to connect to non-existent proxy at 127.0.0.1:9999

**Flow:**

1. Client attempts connection
2. Connection refused immediately
3. Error propagated before LLM invocation

**Expected Result:**

- Connection fails with error
- Error message contains "connect" or "refused"
- No LLM calls made

**LLM Calls:** 0

### Test 3: Missing Required Parameter

**Test:** `test_socks5_client_missing_target_addr`

**Setup:**

- Client created without `target_addr` startup parameter

**Flow:**

1. Client attempts to connect
2. Parameter validation fails
3. Error returned immediately

**Expected Result:**

- Connection fails with error
- Error message contains "target_addr" or "missing"
- No LLM calls made

**LLM Calls:** 0

## Test Runtime

**Expected Runtime:** 10-15 seconds per test

**Breakdown:**

- Server startup: 0.1 seconds
- Connection establishment: 0.5 seconds
- LLM processing: 3-8 seconds per call
- Cleanup: 0.1 seconds

**Total Suite Runtime:** ~30-45 seconds (all tests combined)

## Test Infrastructure Details

### Custom SOCKS5 Proxy Implementation

**Protocol Support:**

- SOCKS5 version (0x05)
- No authentication (0x00)
- Username/password authentication (0x02) - future
- CONNECT command only (not BIND or UDP ASSOCIATE)
- IPv4 addresses (0x01)
- Domain names (0x03)

**Handshake Sequence:**

```
1. Client → Proxy: [0x05, 0x01, 0x00] (version 5, 1 method, no auth)
2. Proxy → Client: [0x05, 0x00] (version 5, method 0 selected)
3. Client → Proxy: [0x05, 0x01, 0x00, ATYP, DST.ADDR, DST.PORT]
   - ATYP: 0x01 (IPv4) or 0x03 (domain name)
4. Proxy → Client: [0x05, 0x00, 0x00, 0x01, BND.ADDR, BND.PORT]
   - 0x00 = success, 0x05 = connection refused
5. Data relay begins (bidirectional copy)
```

**Proxy Behavior:**

- Listens on random port (assigned dynamically)
- Accepts CONNECT requests for any target
- Relays data bidirectionally between client and target
- Closes connection when either side disconnects

### Echo Server Implementation

**Behavior:**

- Simple TCP echo server
- Reflects all received bytes back to sender
- Supports multiple concurrent connections
- Closes connection on EOF

### Helper Functions

**`find_available_port()`**

- Binds to `127.0.0.1:0` to get OS-assigned port
- Returns port number
- Ensures no port conflicts in tests

**`start_echo_server()`**

- Spawns echo server on random port
- Returns port number
- Runs in background task

**`start_socks5_proxy_no_auth()`**

- Spawns SOCKS5 proxy on random port
- Returns port number
- Runs in background task

## Known Issues

### Flaky Test Conditions

1. **Port Conflicts**
    - **Issue:** Random port selection can occasionally conflict
    - **Mitigation:** Use OS-assigned ports (bind to :0)
    - **Likelihood:** Very low (<1%)

2. **Timing Issues**
    - **Issue:** LLM processing time varies (3-10 seconds)
    - **Mitigation:** 10-second timeout with polling
    - **Likelihood:** Low if Ollama is responsive

3. **Ollama Unavailable**
    - **Issue:** Tests fail if Ollama not running
    - **Mitigation:** Tests marked with `#[ignore]`, must run explicitly
    - **Likelihood:** High if Ollama not installed

### Test Limitations

1. **No Authentication Tests**
    - Username/password authentication not tested yet
    - Proxy implementation supports it (commented out in tests)
    - Can be added in future with < 3 additional LLM calls

2. **No UDP ASSOCIATE Tests**
    - SOCKS5 UDP functionality not tested
    - Client implementation doesn't support UDP (TCP only)
    - Out of scope for current implementation

3. **No IPv6 Tests**
    - Only IPv4 and domain names tested
    - tokio-socks supports IPv6
    - Can be added with 0 additional LLM calls (just address format)

## Running Tests

### Run All SOCKS5 Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features socks5 --test client::socks5::e2e_test
```

### Run Specific Test

```bash
./cargo-isolated.sh test --no-default-features --features socks5 --test client::socks5::e2e_test test_socks5_client_no_auth_basic -- --ignored
```

### Prerequisites

- Ollama running at `http://localhost:11434`
- Model `qwen3-coder:30b` available
- `--ignored` flag required (tests marked with `#[ignore]`)

## Success Criteria

**Test passes if:**

1. Connection establishes through SOCKS5 proxy ✅
2. Data flows bidirectionally through tunnel ✅
3. LLM receives events and executes actions ✅
4. Client status updates correctly ✅
5. Error cases handled gracefully ✅
6. Total LLM calls < 10 ✅

**Test fails if:**

- Connection fails when proxy is running
- Data not transmitted through tunnel
- LLM errors out (prompt issues)
- Client status incorrect
- Test hangs (timeout > 15 seconds)

## Future Enhancements

**Potential Additions (if needed):**

1. **Authentication Test** (+2 LLM calls)
    - Test username/password authentication
    - Verify auth failure handling

2. **Connection Reuse Test** (+1 LLM call)
    - Multiple requests through same tunnel
    - Verify tunnel persistence

3. **Large Data Transfer Test** (+1 LLM call)
    - Test with 10KB+ payloads
    - Verify chunking and reassembly

4. **Concurrent Tunnels Test** (+3 LLM calls)
    - Multiple SOCKS5 clients through same proxy
    - Verify isolation

5. **Proxy Chaining Test** (+2 LLM calls)
    - Client → Proxy1 → Proxy2 → Target
    - Verify multi-hop tunneling

**Total Potential LLM Calls:** 9 additional (18 total)
Still under reasonable budget for comprehensive testing.

## Debugging

**Enable Trace Logging:**

```bash
RUST_LOG=netget=trace ./cargo-isolated.sh test --no-default-features --features socks5 --test client::socks5::e2e_test -- --nocapture --ignored
```

**Check netget.log:**

```bash
tail -f netget.log | grep -i socks
```

**Common Issues:**

1. **Connection Refused**
    - Proxy not started (check `start_socks5_proxy_no_auth()` called)
    - Port conflict (rare with OS-assigned ports)

2. **LLM Timeout**
    - Ollama not running → start Ollama
    - Model not loaded → `ollama pull qwen3-coder:30b`
    - Slow response → increase timeout in test

3. **Test Hangs**
    - Proxy deadlock (shouldn't happen with tokio::io::copy)
    - LLM waiting indefinitely (check prompt format)

## CI/CD Considerations

**For CI Pipelines:**

1. **Ollama Setup**
    - Install Ollama in CI environment
    - Pull model before tests: `ollama pull qwen3-coder:30b`
    - Start Ollama service: `ollama serve &`

2. **Test Isolation**
    - Use `cargo-isolated.sh` for separate target directories

3. **Timeout Settings**
    - Increase LLM timeout for slower CI machines
    - Use smaller model for faster CI (e.g., `qwen3-coder:7b`)

4. **Resource Limits**
    - Each test uses ~100MB RAM
    - LLM calls need ~2GB VRAM (GPU) or 8GB RAM (CPU)
    - Estimated CI time: 2-3 minutes (with warm Ollama)
