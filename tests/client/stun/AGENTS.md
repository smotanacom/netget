# STUN Client E2E Tests

## Test Strategy

E2E tests for STUN client using public Google STUN servers. Tests verify NAT traversal discovery by making real binding
requests and parsing responses.

## LLM Call Budget

**Target:** < 10 calls
**Actual:** 3 calls (3 E2E tests)

## Tests

1. **test_stun_client_discover_external_address** (1 LLM call)
    - Connect to stun.l.google.com:19302
    - Send binding request
    - Verify external address discovery
    - Runtime: ~2 seconds

2. **test_stun_client_alternative_server** (1 LLM call)
    - Connect to stun1.l.google.com:19302
    - Verify STUN protocol detection
    - Runtime: ~2 seconds

3. **test_stun_client_binding_response** (1 LLM call)
    - Send binding request
    - Process binding response
    - Verify external IP/port parsing
    - Runtime: ~2 seconds

## Runtime

**Expected:** < 10 seconds total for all tests

## Public STUN Servers Used

- `stun.l.google.com:19302` (primary)
- `stun1.l.google.com:19302` (alternative)

These are Google's free public STUN servers, reliable and always available.

## Test Approach

Tests are **black-box** E2E tests that:

1. Spawn NetGet binary with STUN client instruction
2. Wait for binding request/response
3. Verify output contains expected protocol/message indicators
4. Do NOT parse actual external addresses (privacy)

## Known Issues

None currently. STUN protocol is simple and reliable.

## Future Tests

- Test with multiple STUN servers for comparison
- Test RFC 5780 NAT Behavior Discovery (requires library update)
- Test IPv6 STUN servers
- Test binding refresh/periodic queries
- Test error handling (unreachable STUN server)
