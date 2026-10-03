# HTTP Proxy Client E2E Testing

## Test Strategy

Black-box, prompt-driven testing using the actual NetGet binary. LLM interprets prompts and controls proxy client
behavior. Tests validate HTTP CONNECT tunneling and data transmission through proxies.

## LLM Call Budget

**Maximum: 10 LLM calls per test suite** (target: ~6)

### Per-Test Breakdown

1. **test_http_proxy_client_connect_and_tunnel**: 3 LLM calls
    - 1 call: Start HTTP proxy server
    - 1 call: Start target HTTP server
    - 1 call: Start proxy client with tunnel instruction

2. **test_http_proxy_client_basic_connection**: 2 LLM calls
    - 1 call: Start minimal TCP server (proxy simulation)
    - 1 call: Start proxy client

3. **test_http_proxy_client_raw_data**: 2 LLM calls
    - 1 call: Start TCP server
    - 1 call: Start proxy client with data send instruction

4. **command_channel_test::injected_http_proxy_actions_reach_the_proxy_socket**: 0 LLM calls
    - raw tokio listener as the proxy; `send_to_client` injects `send_http_request`,
      `send_data` and the Custom-result `establish_tunnel`; bad action rejected; `disconnect`
      half-closes and drops the handle

**Total: 7 LLM calls** (within budget)

## Runtime Expectations

- **Per test**: 2-4 seconds (includes 500ms-2s sleep for connection establishment)
- **Full suite**: ~10 seconds
- **Considerations**: Network I/O, CONNECT handshake, LLM response time

## Test Infrastructure

### Dependencies

- NetGet binary compiled with `--features http_proxy`
- Helper functions from `tests/helpers/mod.rs`:
    - `start_netget_server()`: Spawn server instances
    - `start_netget_client()`: Spawn client instances
    - `NetGetConfig`: Configure test instances

### Test Environment

- **Localhost only**: All tests use 127.0.0.1
- **Dynamic ports**: Use `{AVAILABLE_PORT}` placeholder
- **No external services**: Self-contained tests

## Test Coverage

### Core Functionality

1. **Connection Establishment**
    - TCP connection to proxy server
    - `http_proxy_connected` event fires
    - LLM receives connection confirmation

2. **CONNECT Tunnel**
    - Send CONNECT request with target host:port
    - Parse 200 Connection established response
    - `http_proxy_tunnel_established` event fires

3. **Data Transmission**
    - Send HTTP requests through tunnel
    - Send raw data (hex-encoded) through tunnel
    - Receive responses via `http_proxy_response_received` event

### Edge Cases

- Connection failures (not yet tested)
- Proxy authentication (not yet implemented)
- Non-200 CONNECT responses (not yet tested)

## Test Scenarios

### Scenario 1: Full Proxy Chain

```
Client -> HTTP Proxy -> Target Server
```

- Client connects to NetGet HTTP proxy server
- Proxy forwards CONNECT to target HTTP server
- Client sends GET request through tunnel
- Validates end-to-end communication

### Scenario 2: Minimal Proxy

```
Client -> Minimal TCP Proxy (responds to CONNECT)
```

- Simpler test using TCP server as proxy
- TCP server responds to CONNECT with 200
- Validates CONNECT handshake only

### Scenario 3: Raw Data Tunnel

```
Client -> Proxy -> (tunnel) -> Raw data
```

- Tests non-HTTP data transmission
- Sends hex-encoded data through tunnel
- Validates opaque tunnel behavior

## Known Issues

### Test Limitations

1. **No real proxy server**: Tests use NetGet's own proxy implementation or minimal TCP server, not external proxies
   like Squid/tinyproxy
2. **No authentication testing**: Proxy-Authorization not yet implemented
3. **No error case testing**: Only happy path tested (200 responses)
4. **No performance testing**: No load testing or connection pooling

### Potential Flaky Tests

- **Timing-dependent**: Uses fixed sleep durations
    - Mitigation: Generous sleep times (500ms-2s)
    - Future: Poll for connection status instead

- **Port conflicts**: Dynamic port allocation may conflict
    - Mitigation: Use `{AVAILABLE_PORT}` helper
    - Future: Retry on EADDRINUSE

## Future Test Enhancements

1. **Proxy Authentication**
    - Test Basic auth (username:password)
    - Test 407 Proxy Authentication Required

2. **Error Handling**
    - Test CONNECT failures (502, 503, etc.)
    - Test connection timeouts
    - Test proxy disconnection mid-tunnel

3. **Integration with External Proxies**
    - Use actual Squid/tinyproxy instances
    - Test against public HTTP proxies
    - Validate compatibility

4. **Performance Tests**
    - Measure tunnel establishment time
    - Test concurrent tunnels
    - Test large data transfers

5. **Chained Proxies**
    - Test Client -> Proxy1 -> Proxy2 -> Target
    - Validate nested CONNECT requests

## Running Tests

### Single Protocol

```bash
./cargo-isolated.sh test --no-default-features --features http_proxy \
    --test client::http_proxy::e2e_test
```

### With Debugging

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features \
    --features http_proxy --test client::http_proxy::e2e_test -- --nocapture
```

### Full Test Suite (All Protocols)

```bash
./cargo-isolated.sh test --all-features
```

## Test Maintenance

- **Update on protocol changes**: If HTTP proxy client behavior changes, update tests
- **Monitor LLM call count**: Ensure tests stay under 10 calls
- **Check timing assumptions**: Adjust sleep durations if tests become flaky
- **Validate against real proxies**: Periodically test with actual proxy servers

## Debugging Tips

1. **Check LLM prompts**: Ensure instructions are clear for tunnel establishment
2. **Inspect network traffic**: Use tcpdump/Wireshark to see CONNECT requests
3. **Enable trace logging**: `RUST_LOG=trace` shows full data flow
4. **Increase sleep times**: If tests fail, try longer sleep durations
5. **Manual testing**: Use `curl --proxy http://127.0.0.1:PORT` to verify proxy behavior

## References

- RFC 7231: HTTP CONNECT method
- Squid proxy documentation
- `tests/helpers/mod.rs`: Test infrastructure
