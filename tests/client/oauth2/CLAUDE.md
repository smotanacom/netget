# OAuth2 Client E2E Testing

## What actually runs

**One file, and it is not this one's subject.** `command_channel_test.rs` drives the
dashboard's injected-action path against an OAuth2 provider served in-process, and it is the
only real coverage this client has.

In `e2e_test.rs`, four of the five tests are dead **twice over**:

- each carries `#[ignore] // Requires Ollama to be running`, so a normal run skips them; and
- each opens with `#[cfg(not(feature = "mcp"))] { println!("Skipping test …"); return; }`,
  because the axum mock server they need arrives with the `mcp` feature. That is the
  skip-when-missing shape the root `CLAUDE.md` calls out: on a build without `mcp` it is a
  *silent pass*, not a skip, so even `--include-ignored` proves nothing there.

The fifth, `test_oauth2_client_initialization`, constructs a `ClientInstance` and asserts it
was added to `AppState`. It contacts nothing and exercises no OAuth2 code.

So the budget and runtime figures below describe tests that do not run. They are kept because
they document what the four *would* cover if converted, which is the useful part — but do not
read them as coverage.

**To convert them**, copy `start_provider` from
`tests/client/openidconnect/command_channel_test.rs`: ~50 lines of `tokio::net::TcpListener`
speaking HTTP/1.1, needing no `mcp` feature and no Ollama, driven through
`AppState::send_to_client` rather than through the model. That removes both gates at once.
Whatever else changes, a skip must **fail** rather than return `Ok(())` — `npm`'s real-CLI
test is the shape.

Unlike the `openidconnect` client's deleted suite, nothing here points at a public provider:
every URL is `localhost`. These are dead, not dangerous.

## Test Strategy (aspirational — see above)

**Approach**: Black-box E2E testing with mock OAuth2 server

**Rationale**: OAuth2 flows involve HTTP requests to provider endpoints. Testing against a real provider introduces
dependencies and rate limits. A mock server provides:

1. Full control over responses
2. Fast, reliable tests
3. No external dependencies
4. Ability to test error scenarios

## LLM Call Budget

**Target**: < 10 LLM calls per test suite

**Breakdown**:

1. **Password Flow Test** (2 calls)
    - Call 1: Initialize client, trigger password exchange
    - Call 2: Process token obtained event

2. **Client Credentials Flow Test** (2 calls)
    - Call 1: Initialize client, trigger credentials exchange
    - Call 2: Process token obtained event

3. **Token Refresh Test** (3 calls)
    - Call 1: Initial authentication
    - Call 2: Token obtained event
    - Call 3: Refresh token event

4. **Error Handling Test** (2 calls)
    - Call 1: Initialize client, trigger invalid auth
    - Call 2: Process error event

**Total**: 9 LLM calls (within budget)

**Optimization Strategies**:

- Use single client instance where possible
- Test device code flow manually (not automated, requires polling)
- Skip authorization code flow in automated tests (requires browser)

## Expected Runtime

**Target**: < 30 seconds for full test suite

**Breakdown**:

- Mock server startup: < 1 second
- Password flow test: ~5 seconds (LLM + HTTP)
- Client credentials test: ~5 seconds
- Token refresh test: ~5 seconds
- Error handling test: ~5 seconds
- Mock server shutdown: < 1 second

**Assumptions**:

- Local Ollama instance with fast model (qwen3-coder:30b or similar)
- Mock OAuth2 server responds instantly
- No external network dependencies

## Test Infrastructure

### Mock OAuth2 Server

**Implementation**: ✅ **COMPLETED**

- Uses `axum` (from mcp feature) to create simple token endpoint
- Responds to POST `/oauth/token` with mock tokens
- Supports all grant_types: password, client_credentials, refresh_token, device_code
- Dynamically allocates ports to avoid conflicts
- Separate error server for testing error scenarios

**Features**:

- `start_mock_oauth_server()` - Returns different tokens based on grant type
- `start_mock_oauth_server_with_errors()` - Returns 400 errors for testing error handling
- Automatic port allocation using `TcpListener::bind("127.0.0.1:0")`
- Conditional compilation with `#[cfg(feature = "mcp")]`

### Test Utilities

**Helper Functions**:

- `start_mock_oauth_server()` - Spawn mock server, return URL
- `stop_mock_oauth_server()` - Graceful shutdown
- `assert_token_stored(client_id)` - Verify token in protocol_data
- `extract_access_token(client_id)` - Get token for assertions

## Test Scenarios

### Test 1: Password Flow

**Setup**:

1. Start mock OAuth2 server
2. Open OAuth2 client with password flow instruction

**Execution**:

1. LLM triggers `exchange_password` action
2. Client sends POST to `/oauth/token` with grant_type=password
3. Mock server returns access token

**Assertions**:

- Client status is Connected
- Access token stored in protocol_data
- Token type is "Bearer"
- Expires_in is set
- LLM received `oauth2_token_obtained` event

**LLM Calls**: 2

### Test 2: Client Credentials Flow

**Setup**:

1. Reuse mock OAuth2 server
2. Open OAuth2 client with client credentials instruction

**Execution**:

1. LLM triggers `exchange_client_credentials` action
2. Client sends POST to `/oauth/token` with grant_type=client_credentials
3. Mock server returns access token (no refresh token)

**Assertions**:

- Access token stored
- No refresh token (expected for client credentials)
- Correct scopes granted

**LLM Calls**: 2

### Test 3: Token Refresh

**Setup**:

1. Reuse mock OAuth2 server
2. Authenticate with password flow to get initial tokens

**Execution**:

1. LLM triggers `refresh_token` action
2. Client sends POST to `/oauth/token` with grant_type=refresh_token
3. Mock server returns new access token

**Assertions**:

- New access token stored
- Old access token replaced
- Refresh token may be rotated

**LLM Calls**: 3 (initial auth + token obtained + refresh)

### Test 4: Error Handling

**Setup**:

1. Configure mock server to return error
2. Open OAuth2 client with invalid credentials

**Execution**:

1. LLM triggers authentication
2. Client sends request
3. Mock server returns `{"error": "invalid_grant", "error_description": "Invalid credentials"}`

**Assertions**:

- Client status remains Connected (error doesn't disconnect)
- LLM received `oauth2_error` event
- Error details captured

**LLM Calls**: 2

### Test 5: Device Code Flow (Manual)

**Note**: Not automated due to polling complexity and timing

**Manual Test Steps**:

1. Start OAuth2 client with device code flow
2. Verify verification URL and user code displayed
3. Simulate user authorization (mock server marks code as authorized)
4. Wait for automatic polling to complete
5. Verify token obtained

**LLM Calls**: N/A (manual test)

### Test 6: Authorization Code Flow (Manual)

**Note**: Not automated due to browser redirect requirement

**Manual Test Steps**:

1. Generate authorization URL with `generate_auth_url`
2. Manually visit URL (or use HTTP client to fetch)
3. Extract authorization code from callback
4. Use `exchange_code` action to trade code for token

**LLM Calls**: N/A (manual test)

## Known Issues

1. **Device Code Polling**: Background polling task may not be properly cleaned up in tests
    - **Mitigation**: Use timeout to stop polling automatically

2. **PKCE Randomness**: PKCE verifier is random, hard to test deterministically
    - **Mitigation**: Test that PKCE fields are populated, not specific values

3. **Token Expiration**: Time-based expiration not tested
    - **Mitigation**: Manual test or long-running test (not in CI)

4. **Provider Compatibility**: Different providers have different requirements
    - **Mitigation**: Document provider-specific quirks in implementation CLAUDE.md

## Flaky Test Mitigation

**Potential Issues**:

- Network timeouts (mock server startup)
- LLM response latency
- Race conditions in polling

**Mitigations**:

- Increase test timeouts to 30 seconds per test
- Retry mock server startup if bind fails
- Use explicit waits for async operations
- Disable device code polling in unit tests

## Test Execution

```bash
# Run OAuth2 client E2E tests
./cargo-isolated.sh test --no-default-features --features oauth2 --test client::oauth2::e2e_test

# Run with verbose logging
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features oauth2 --test client::oauth2::e2e_test -- --nocapture
```

## Feature Gating

All tests MUST be feature-gated:

```rust
#[cfg(all(test, feature = "oauth2"))]
mod tests {
    // Tests here
}
```

## Dependencies for Tests

Test-only dependencies (in `[dev-dependencies]`):

- `axum` - Mock HTTP server
- `tower-http` - Middleware for axum
- Existing: `tokio-test`, `ctor`

## Success Criteria

1. All tests pass with < 10 LLM calls total
2. Test suite completes in < 30 seconds
3. No external dependencies (localhost only)
4. Tests are deterministic (no flakes)
5. Proper cleanup (no leaked resources)

## Future Improvements

1. **Add More Flows**: Test authorization code flow with embedded callback server
2. **Error Scenarios**: Test network errors, timeout errors, malformed responses
3. **Token Expiration**: Test automatic refresh on expiration
4. **Multiple Providers**: Test compatibility with Google, GitHub, Auth0
5. **Concurrent Clients**: Test multiple OAuth2 clients simultaneously
