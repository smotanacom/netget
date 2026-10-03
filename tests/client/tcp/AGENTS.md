# TCP Client E2E Tests

## Test Strategy

Black-box E2E tests using real TCP server (nc/netcat).

## LLM Call Budget

**Target:** < 10 calls total
**Actual:** ~3 calls (connection test, data exchange test)

## Test Server Setup

```bash
# Terminal 1: Start test server
nc -l 9000

# Terminal 2: Run tests
./cargo-isolated.sh test --no-default-features --features tcp --test client::tcp::e2e_test
```

## Tests

1. **test_tcp_client_connect_and_send** (2 LLM calls)
    - Connect to nc server on localhost:9000
    - Verify connection status
    - Send data via LLM action

2. **test_tcp_client_disconnect** (1 LLM call)
    - Connect and gracefully disconnect
    - Verify status transitions

## Runtime

**Expected:** < 30 seconds total

## Known Issues

- Tests marked `#[ignore]` - require manual server setup
- Run with `--ignored` flag when server is ready
