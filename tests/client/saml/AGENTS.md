# SAML Client E2E Test Strategy

## Overview

Tests for SAML client implementation. These tests verify that the LLM can correctly initialize a SAML client, generate
SSO URLs, and parse SAML responses.

## Test Approach

### Black-Box Testing

- Tests spawn NetGet binary with SAML client instructions
- No access to internal state or implementation details
- Validates output and behavior based on LLM responses

### Minimal External Dependencies

- Tests do NOT require a real SAML Identity Provider (IdP)
- Client initialization and SSO URL generation can be tested independently
- Full authentication flow would require mock IdP (future enhancement)

## LLM Call Budget

### Per Test

1. **test_saml_client_initialization**: 1 LLM call
    - Client connection/initialization

2. **test_saml_client_sso_url_generation**: 2 LLM calls
    - Client connection
    - SSO initiation action

### Total Budget: 3 LLM calls

- Well under the 10 call limit for E2E tests
- Allows for additional tests if needed

## Expected Runtime

- **test_saml_client_initialization**: ~2 seconds
    - Client startup: ~500ms
    - LLM processing: ~1-1.5s

- **test_saml_client_sso_url_generation**: ~3 seconds
    - Client startup: ~500ms
    - LLM processing for SSO: ~2-2.5s

**Total**: ~5 seconds for all tests

## Test Coverage

### Currently Covered

1. ✅ Client initialization
2. ✅ Protocol detection (SAML)
3. ✅ SSO URL generation
4. ✅ LLM instruction parsing

### Not Covered (Future Tests)

1. ❌ Full authentication flow (requires mock IdP)
2. ❌ Assertion validation with real SAML response
3. ❌ Attribute extraction from assertions
4. ❌ Error handling for invalid responses
5. ❌ Different SAML bindings (HTTP-POST)

## Known Issues

### Test Limitations

1. **No Real IdP**: Tests don't connect to a real SAML IdP
    - Avoids external dependencies
    - Cannot test full authentication flow
    - Future: Could add mock IdP server

2. **No Signature Verification**: Tests don't verify XML signatures
    - Current implementation doesn't support signature verification
    - Requires xmlsec integration (complex)

3. **Basic Validation Only**: Tests verify basic output presence
    - Don't validate SAML protocol compliance
    - Don't check XML structure validity

### Flaky Test Risks

1. **LLM Response Variance**: LLM may phrase output differently
    - Tests use flexible assertions (multiple keywords)
    - May need adjustment for different LLM models

2. **Timing**: Async operations may need more time
    - Tests use conservative sleep durations
    - Adjust if tests become flaky

## Running Tests

### Single Protocol Test

```bash
./cargo-isolated.sh test --no-default-features --features saml --test client::saml::e2e_test
```

### With Other Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features tcp,http,redis,saml --test 'client::*::e2e_test'
```

### All Tests (Slow)

```bash
./cargo-isolated.sh test --all-features
```

## Mock IdP Setup (Future)

For comprehensive testing, consider adding a mock SAML IdP:

### Option 1: SimpleSAMLphp Docker

```bash
docker run -d -p 8080:8080 \
  kristophjunge/test-saml-idp
```

### Option 2: Embedded Mock Server

- Implement a minimal SAML IdP in test helpers
- Generate signed SAML responses
- Would increase test complexity but enable full flow testing

### Option 3: Static Response Files

- Store example SAML responses in test fixtures
- Test assertion parsing without live IdP
- Simplest approach for validation testing

## Privacy & Security

### Test Data

- Uses fake IdP URLs (idp.example.com)
- No real authentication credentials
- No external network calls in current tests

### Future Considerations

- If adding real IdP integration, use localhost only
- Never commit real credentials or certificates
- Mock sensitive data (private keys, certificates)

## Maintenance Notes

### When to Update Tests

1. **SAML Protocol Changes**: Update if SAML spec implementation changes
2. **LLM Model Updates**: May need to adjust assertions for new LLM behaviors
3. **Action Changes**: Update if SAML client actions are modified

### Breaking Changes

- Changing SAML binding format would break SSO URL tests
- Modifying event structure would affect assertion tests
- Protocol keyword changes would break initialization tests

## References

- See `src/client/saml/CLAUDE.md` for implementation details
- [SAML 2.0 Test IdP](https://samltest.id/) - For manual testing
- [SimpleSAMLphp](https://simplesamlphp.org/) - For mock IdP setup
