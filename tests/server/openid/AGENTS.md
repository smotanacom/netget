# OpenID Connect E2E Test Documentation

## Test Strategy

The OpenID Connect E2E tests validate the full OIDC provider flow using HTTP requests from a standard HTTP client (
reqwest). Tests cover all core OIDC endpoints and error handling.

## Test Organization

### Test 1: Full OIDC Flow (`test_openid_connect_flow`)

Tests the complete OpenID Connect authentication flow:

1. **Discovery Endpoint** - Validates provider metadata
2. **Authorization Endpoint** - Tests redirect-based authorization code flow
3. **Token Endpoint** - Validates token exchange (code → tokens)
4. **UserInfo Endpoint** - Tests authenticated user profile retrieval
5. **JWKS Endpoint** - Validates public key distribution

**LLM Calls**: 5 (one per endpoint)

### Test 2: Error Handling (`test_openid_error_handling`)

Tests OAuth/OIDC error responses:

1. Invalid authorization request (missing parameters)
2. Unsupported grant type

**LLM Calls**: 2

**Total LLM Calls**: 7 (well under the 10-call budget)

## Runtime

- **Expected Duration**: 15-25 seconds
- **Timeout**: 30 seconds per test
- **Bottleneck**: LLM response time (5-7 calls × 2-3s each)

## Testing Approach

### Black-Box Validation

Tests use standard HTTP requests without knowledge of internal implementation:

- `reqwest` HTTP client for all requests
- JSON parsing for response validation
- Standard OIDC flow (RFC 6749, OpenID Connect Core 1.0)

### Endpoint Coverage

#### 1. Discovery (/.well-known/openid-configuration)

- **Method**: GET
- **Validates**: Issuer, endpoint URLs, supported features
- **Key Assertions**:
    - All endpoints present
    - Correct issuer URL
    - Supported scopes and response types

#### 2. Authorization (/authorize)

- **Method**: GET with query parameters
- **Flow**: Redirect-based authorization code flow
- **Validates**: 302 redirect with authorization code
- **Key Assertions**:
    - Location header contains code
    - State parameter preserved
    - Redirect URI matches

#### 3. Token (/token)

- **Method**: POST with form-encoded body
- **Validates**: Token exchange (code → access_token + id_token)
- **Key Assertions**:
    - access_token present
    - id_token (JWT) present
    - token_type is "Bearer"
    - expires_in is reasonable (3600s)
    - scope matches requested

#### 4. UserInfo (/userinfo)

- **Method**: GET with Authorization header
- **Validates**: User profile retrieval
- **Key Assertions**:
    - sub (subject) matches
    - Standard claims (name, email) present
    - email_verified is boolean

#### 5. JWKS (/jwks.json)

- **Method**: GET
- **Validates**: Public key distribution
- **Key Assertions**:
    - keys array present
    - RSA key structure (kty, use, alg, n, e)
    - Key suitable for JWT verification

### Error Scenarios

#### Invalid Authorization Request

- **Trigger**: Missing client_id or redirect_uri
- **Expected**: 400 Bad Request + JSON error
- **Validates**: OAuth error format

#### Unsupported Grant Type

- **Trigger**: grant_type other than "authorization_code"
- **Expected**: 400 Bad Request + "unsupported_grant_type" error
- **Validates**: Token endpoint validation

## LLM Call Budget

| Test                       | Endpoint      | LLM Calls | Cumulative |
|----------------------------|---------------|-----------|------------|
| test_openid_connect_flow   | Discovery     | 1         | 1          |
| test_openid_connect_flow   | Authorization | 1         | 2          |
| test_openid_connect_flow   | Token         | 1         | 3          |
| test_openid_connect_flow   | UserInfo      | 1         | 4          |
| test_openid_connect_flow   | JWKS          | 1         | 5          |
| test_openid_error_handling | Invalid Auth  | 1         | 6          |
| test_openid_error_handling | Invalid Token | 1         | 7          |
| **Total**                  |               | **7**     | **7/10**   |

✅ Budget: **70% utilized** (7 out of 10 allowed calls)

## Known Issues / Limitations

1. **JWT Validation**: Tests do NOT verify JWT signature (would require cryptographic library)
    - Tests only check JWT structure and claims
    - Signature validation can be added in future with `jsonwebtoken` crate

2. **PKCE**: Proof Key for Code Exchange not tested
    - Future enhancement for security testing

3. **Multiple Clients**: Tests use single client_id
    - Multi-client scenarios could be added

4. **Token Refresh**: Refresh token flow not tested
    - Can be added as separate test

## Running Tests

```bash
# Run all OpenID E2E tests
./cargo-isolated.sh test --no-default-features --features openid --test e2e_test -- --ignored

# Run specific test
./cargo-isolated.sh test --no-default-features --features openid --test e2e_test test_openid_connect_flow -- --ignored
```

## Dependencies

### Production

- `hyper` - HTTP server (already in base dependencies)
- `urlencoding` - Query/form parameter encoding (already in base)
- `serde_json` - JSON serialization (already in base)

### Test-Only

- `reqwest` - HTTP client for E2E tests (already in dev-dependencies)

**No additional dependencies required** ✅

## Debugging

### Enable Trace Logs

```bash
RUST_LOG=netget=trace ./cargo-isolated.sh test --no-default-features --features openid --test e2e_test -- --ignored --nocapture
```

### Common Failures

| Symptom                                       | Likely Cause                    | Fix                                         |
|-----------------------------------------------|---------------------------------|---------------------------------------------|
| Timeout waiting for "OpenID server listening" | Server failed to start          | Check port availability, check logs         |
| 500 Internal Server Error                     | LLM failed to generate response | Check Ollama connection, model availability |
| JSON parse error                              | LLM returned non-JSON           | Improve prompt clarity in test              |
| Assertion failure on redirect                 | LLM didn't include code/state   | Check authorization endpoint logic          |

## Future Enhancements

1. **JWT Signature Verification**: Use `jsonwebtoken` crate to validate id_token signatures
2. **PKCE Support**: Test authorization with code_challenge/code_verifier
3. **Multiple Flows**: Test implicit flow, hybrid flow
4. **Client Authentication**: Test client_secret, JWT assertions
5. **Token Introspection**: Add /introspect endpoint tests
6. **Token Revocation**: Add /revoke endpoint tests
7. **Dynamic Client Registration**: Test /register endpoint
