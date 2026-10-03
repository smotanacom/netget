# TLS Client E2E Tests

## Test Strategy

Black-box E2E tests using NetGet TLS server (self-signed certificates) and public HTTPS servers (valid certificates).

## LLM Call Budget

**Target:** < 10 calls total across all tests
**Actual:** ~7 calls (3 tests × ~2-3 calls each)

### Per-Test Breakdown

1. **test_tls_client_connect_to_server** - 4 LLM calls
   - Server startup mock (1 call)
   - Client startup mock (1 call)
   - Server data received (1 call)
   - Client connected (1 call)

2. **test_tls_client_certificate_validation** - 2 LLM calls
   - Client startup mock (1 call)
   - Client connected to public HTTPS (1 call)

3. **test_tls_client_rejects_self_signed_cert** - 1 LLM call
   - Client startup mock (1 call - connection fails before connected event)

**Total: 7 LLM calls** (within budget)

## Test Coverage

### 1. TLS Handshake

- ✅ Connect to TLS server with self-signed cert (accept_invalid_certs: true)
- ✅ Connect to public HTTPS with valid cert (accept_invalid_certs: false)
- ✅ Reject self-signed cert when validation enabled
- ❌ mTLS (mutual TLS) - not implemented
- ❌ TLS 1.2 vs 1.3 selection - uses rustls defaults

### 2. Data Exchange

- ✅ Send UTF-8 data over TLS
- ✅ Receive encrypted data from server
- ✅ Echo test (send data, get echo back)
- ❌ Binary data (hex encoding) - tested via implementation but not E2E

### 3. Certificate Validation

- ✅ Accept self-signed certificates (testing mode)
- ✅ Validate against webpki roots (production mode)
- ✅ Reject invalid certificates when validation enabled
- ❌ Custom CA certificates - not implemented
- ❌ Certificate pinning - not implemented

### 4. SNI (Server Name Indication)

- ✅ Auto-detect SNI from hostname (example.com:443 → SNI: example.com)
- ❌ Custom SNI override - tested via implementation but not E2E

## Running Tests

### All TLS Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features tls --test client::tls::e2e_test
```

### Single Test

```bash
./cargo-isolated.sh test --no-default-features --features tls --test client::tls::e2e_test -- test_tls_client_connect_to_server
```

### With Real Ollama (No Mocks)

```bash
./cargo-isolated.sh test --no-default-features --features tls --test client::tls::e2e_test -- --use-ollama
```

## Test Infrastructure

### NetGet TLS Server

Tests spawn a real NetGet TLS server with self-signed certificates:
- Certificate generated via `rcgen`
- CN: "netget-dns-server"
- Validity: 365 days
- Used for testing `accept_invalid_certs: true` mode

### Public HTTPS Server

Tests connect to `example.com:443`:
- Valid certificate signed by trusted CA
- Used for testing `accept_invalid_certs: false` mode
- Requires internet connection

## Runtime

**Expected:** < 45 seconds total
- ~15s per test (TLS handshake overhead)
- Parallel execution: ~20s total with `--test-threads=100`

## Known Issues

### 1. Test Flakiness

- **Public HTTPS test** may fail due to network issues (example.com unreachable)
- **Workaround**: Retry or skip if network unavailable

### 2. TLS Handshake Timeouts

- TLS handshake can take longer than TCP (2-5 seconds vs instant)
- **Solution**: Increased timeout to 10-15 seconds for TLS tests

### 3. Certificate Expiry

- Self-signed certs generated at runtime (no expiry issues)
- Public HTTPS certs may expire (example.com renews regularly)

## Debugging Tips

### View TLS Logs

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features tls --test client::tls::e2e_test
```

### Inspect TLS Handshake

Look for these log patterns:
- `TLS client {} TCP connected` - TCP connection established
- `TLS handshake complete` - Successful TLS handshake
- `TLS handshake failed` - Certificate validation or protocol error

### Check Certificate Validation

```bash
# Should succeed (valid cert)
openssl s_client -connect example.com:443 -CAfile /etc/ssl/certs/ca-certificates.crt

# Should fail (self-signed cert)
openssl s_client -connect localhost:8443 -CAfile /etc/ssl/certs/ca-certificates.crt
```

## Mock Testing

All tests use mocks by default (no Ollama required):
- Server startup mocked
- Client connection mocked
- Data exchange mocked
- Actual TLS handshake and encryption still happen (not mocked)

Run with `--use-ollama` to test with real LLM:
```bash
./cargo-isolated.sh test --no-default-features --features tls --test client::tls::e2e_test -- --use-ollama
```

## Future Tests

### Not Yet Implemented

1. **TLS Session Resumption** - Reconnect reusing session
2. **Client Certificates (mTLS)** - Mutual TLS authentication
3. **Custom CA Certificates** - Load CA bundle from file
4. **TLS Version Negotiation** - Force TLS 1.2 vs 1.3
5. **Cipher Suite Selection** - Test specific cipher suites
6. **ALPN (Application-Layer Protocol Negotiation)** - HTTP/2, HTTP/3 selection
7. **SNI with Multiple Hostnames** - Virtual hosting scenarios

### Rationale for Exclusion

- **Complexity**: mTLS, custom CAs require complex setup
- **Low Priority**: ALPN, cipher selection less critical for generic TLS client
- **Library Limitations**: rustls handles most of this automatically
