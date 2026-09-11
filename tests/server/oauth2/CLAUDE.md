# OAuth2 Protocol E2E Testing

## Overview

End-to-end tests for the OAuth2 authorization server implementation. Tests validate OAuth2 flows (authorization code,
client credentials) and token management (introspection, revocation) using HTTP clients.

## Test Strategy

### Testing Approach

- **Black-box testing**: Tests interact with OAuth2 server via HTTP clients
- **Prompt-driven**: LLM interprets OAuth2 prompts and generates responses
- **RFC compliance**: Validates responses against RFC 6749, RFC 7662, RFC 7009
- **Real clients**: Uses `reqwest` HTTP client to test OAuth2 flows

### Test Coverage

**10 tests across three files.** `e2e_test.rs` covers the happy paths, and the other two cover
the ways this protocol has failed *open* — which is what it is actually known for.

`e2e_test.rs` (4):

1. **Authorization Code Flow** - Full authorization + token exchange flow
2. **Client Credentials Flow** - Service-to-service authentication
3. **Token Introspection** - Token validation (RFC 7662)
4. **Token Revocation** - Token invalidation (RFC 7009)

`llm_failure_test.rs` (4): with no routing rule for any `oauth2_*` event, every endpoint's LLM
call fails. Each test asserts the endpoint answers a 5xx with an RFC 6749 §5.2 code and issues
nothing — and specifically that `/token` does **not** say `invalid_grant` (which makes a
conforming client discard its refresh token, so an outage would sign every session out
permanently) and `/introspect` does **not** say `{"active": false}` (a statement about the
token, when nobody looked at it).

`hardening_test.rs` (2): a 1 MiB `/token` body is refused `413` before any LLM call, and a
model-supplied `status_code` of `65736` does not wrap into a `200`. Both were live defects;
`65736 as u16 == 200` meant a refusal arrived as the status a client reads as success.

### LLM Call Budget

**Target**: < 10 LLM calls total

**Actual LLM Calls**:

- Test 1 (Authorization Code): 2 LLM calls (authorize + token)
- Test 2 (Client Credentials): 1 LLM call (token)
- Test 3 (Token Introspection): 2 LLM calls (2 introspect requests)
- Test 4 (Token Revocation): 1 LLM call (revoke)
- **Total**: 6 LLM calls ✓

### Runtime

**Estimated Total Runtime**: ~45-60 seconds

- Test 1: ~15-20 seconds (2 LLM calls)
- Test 2: ~8-10 seconds (1 LLM call)
- Test 3: ~15-18 seconds (2 LLM calls)
- Test 4: ~8-10 seconds (1 LLM call)

## Test Details

### Test 1: Authorization Code Flow

**Purpose**: Validate full OAuth2 authorization code flow (most common OAuth2 flow)

**Flow**:

1. Client requests authorization (`GET /authorize`)
2. Server returns authorization code via redirect
3. Client exchanges code for access token (`POST /token`)
4. Server returns access token + refresh token

**Validations**:

- Authorization endpoint returns 302 redirect
- Authorization code included in redirect URL
- Token endpoint returns 200 OK with JSON
- Response includes `access_token`, `token_type`, `expires_in`
- Token type is "Bearer"
- Optional `refresh_token` included

**LLM Calls**: 2

### Test 2: Client Credentials Flow

**Purpose**: Validate service-to-service authentication flow

**Flow**:

1. Client sends credentials directly to token endpoint
2. Server validates and returns access token

**Validations**:

- Token endpoint returns 200 OK
- Response includes `access_token` and `token_type`
- Token type is "Bearer"
- No refresh token (not applicable for this grant type)

**LLM Calls**: 1

### Test 3: Token Introspection

**Purpose**: Validate token validation endpoint (RFC 7662)

**Flow**:

1. Client sends token to introspection endpoint
2. Server returns token metadata

**Validations**:

- Introspection endpoint returns 200 OK for all requests
- Valid tokens return `{"active": true}` with metadata
- Invalid tokens return `{"active": false}`
- Response format complies with RFC 7662

**LLM Calls**: 2 (one valid, one invalid)

### Test 4: Token Revocation

**Purpose**: Validate token revocation endpoint (RFC 7009)

**Flow**:

1. Client sends token to revocation endpoint
2. Server acknowledges revocation

**Validations**:

- Revocation endpoint returns 200 OK
- No response body required (per RFC 7009)
- Endpoint accepts both access tokens and refresh tokens

**LLM Calls**: 1

## Client Library

**reqwest**: HTTP client for Rust

- Async/await support
- Form-encoded body support
- Query parameter support
- Redirect handling

## Known Issues

Nothing outstanding in the tests. What the suite does **not** cover, and could: PKCE, scope
enforcement, and the `/authorize` redirect-URI escaping (`authorize_redirect` percent-encodes
every value, but no test drives a `state` containing `&`).

## Test Execution

### Running Tests

```bash
# Run all OAuth2 tests
# `--test` names a target, not a module: the target is `server`, the module is a filter.
./cargo-isolated.sh test --no-default-features --features oauth2 \
    --test server -- server::oauth2 --test-threads=100

# Run specific test
./cargo-isolated.sh test --no-default-features --features oauth2 \
    --test server -- server::oauth2::e2e_test::test_oauth2_authorization_code_flow
```

### Prerequisites

- **No Ollama.** Every test here runs against the in-process mock
  (`tests/helpers/mock_ollama.rs`); `--use-ollama` is opt-in, not required.
- Isolated cargo build environment (`./cargo-isolated.sh`)

### Test Output

Each test prints detailed progress:

```
=== E2E Test: OAuth2 Authorization Code Flow ===
Server started on port 12345

[1/2] Testing authorization endpoint...
✓ Received authorization response: 302 Found
✓ Redirect location: http://localhost:3000/callback?code=AUTH_xyz123&state=random_state_123
✓ Authorization code: AUTH_xyz123

[2/2] Testing token endpoint...
✓ Received token response: 200 OK
Token response: {
  "access_token": "ACCESS_token_456",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "REFRESH_token_789"
}
✓ Access token: ACCESS_token_456
✓ Refresh token: REFRESH_token_789

✓ OAuth2 Authorization Code Flow test completed
```

## Future Enhancements

### Additional Test Cases

- **PKCE Flow**: Test Proof Key for Code Exchange (RFC 7636)
- **Resource Owner Password**: Test password grant type
- **Refresh Token Flow**: Test token refresh
- **Error Responses**: Test invalid client, invalid grant, etc.
- **Scope Validation**: Test scope enforcement
- **Multiple Clients**: Test with different client configurations

### Performance Tests

- Concurrent authorization requests
- Token endpoint throughput
- Large token introspection batches

### Security Tests

- Invalid client credentials
- Expired authorization codes
- Token replay attacks
- CSRF protection (state parameter)

## References

- [RFC 6749: OAuth 2.0 Authorization Framework](https://datatracker.ietf.org/doc/html/rfc6749)
- [RFC 7662: OAuth 2.0 Token Introspection](https://datatracker.ietf.org/doc/html/rfc7662)
- [RFC 7009: OAuth 2.0 Token Revocation](https://datatracker.ietf.org/doc/html/rfc7009)
- [OAuth 2.0 Security Best Current Practice](https://datatracker.ietf.org/doc/html/draft-ietf-oauth-security-topics)
