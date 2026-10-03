# IPSec/IKEv2 Honeypot E2E Tests

## Test Overview

Tests IPSec/IKEv2 honeypot functionality by sending crafted IKE handshake packets to NetGet and verifying reconnaissance
detection.

**Protocol Status**: Honeypot-only (no actual VPN tunnels)
**Test Focus**: IKE packet detection and logging

## Test Strategy

### Consolidated Test Suite

Tests reuse single server instances across multiple scenarios:

- **5 test functions** covering IKEv2/IKEv1, multiple exchange types, concurrency
- Each test spawns server, sends packets, verifies detection

### Packet-Level Testing

No actual IPSec clients used - tests construct raw IKE packets:

- **IKEv2**: IKE_SA_INIT (34), IKE_AUTH (35), CREATE_CHILD_SA (36), INFORMATIONAL (37)
- **IKEv1**: Identity Protection (2), Aggressive Mode (4)

### Disabled Protocol Flag

IPSec is disabled by default. Tests use `--include-disabled-protocols`:

```rust
let config = ServerConfig::new(prompt)
    .with_include_disabled_protocols(true);
```

## LLM Call Budget

### Per-Test Breakdown

1. **test_ipsec_ikev2_sa_init_detection**: 1 LLM call
    - Server startup (prompt interpretation)

2. **test_ipsec_ikev2_auth_detection**: 1 LLM call
    - Server startup

3. **test_ipsec_ikev1_detection**: 1 LLM call
    - Server startup

4. **test_ipsec_multiple_exchange_types**: 1 LLM call
    - Server startup (handles 4 exchange types without additional calls)

5. **test_ipsec_concurrent_connections**: 1 LLM call
    - Server startup (3 concurrent clients, no LLM per-client)

**Total: 5 LLM calls** (well under 10 limit)

### Why So Few Calls?

Honeypot mode logs packets without LLM interpretation. LLM only consulted on startup.

## Scripting Usage

**Scripting: Not applicable** - Honeypot doesn't use scripting. Packets logged directly.

## Client Library

### Manual Packet Construction

**Why manual**: No Rust IPSec/IKE client library suitable for testing.

### IKE Packet Format

**IKE Header (28 bytes)**:

```
| Initiator SPI (8) | Responder SPI (8) |
| Next Payload (1) | Version (1) | Exchange Type (1) | Flags (1) |
| Message ID (4) | Length (4) |
| Payloads (variable) |
```

**IKEv2 Version**: 0x20 (Major=2, Minor=0)
**IKEv1 Version**: 0x10 (Major=1, Minor=0)

### Exchange Types

**IKEv2**:

- 34 (IKE_SA_INIT) - Initial exchange, establish SA
- 35 (IKE_AUTH) - Authentication exchange
- 36 (CREATE_CHILD_SA) - Create new Child SA
- 37 (INFORMATIONAL) - Informational messages

**IKEv1**:

- 2 (Identity Protection) - Main mode
- 4 (Aggressive Mode) - Aggressive mode

### Packet Builders

```rust
fn build_ikev2_sa_init() -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&0x0123456789ABCDEFu64.to_be_bytes());  // Initiator SPI
    packet.extend_from_slice(&0x0000000000000000u64.to_be_bytes());  // Responder SPI (zero)
    packet.push(33);  // Next Payload (SA)
    packet.push(0x20);  // Version (IKEv2)
    packet.push(34);  // Exchange Type (IKE_SA_INIT)
    packet.push(0x08);  // Flags (Initiator)
    packet.extend_from_slice(&0x00000000u32.to_be_bytes());  // Message ID
    // ... length and payloads ...
    packet
}
```

## Expected Runtime

**Model**: qwen3-coder:30b (or configured model)
**Runtime**: ~20-25 seconds for full test suite
**Breakdown**:

- Server startup: 2-5 seconds per test (5 tests)
- Packet sending: <1 second per test
- LLM calls: 2-3 seconds each (startup only)

**Fast because**: No LLM calls for packet handling.

## Failure Rate

**Low-Medium** (5-10%) - Occasional stack selection issues.

**Known flakiness**: LLM sometimes selects generic UDP stack instead of IPSec-specific stack, even with "via ipsec"
keyword.

## Test Cases

### 1. test_ipsec_ikev2_sa_init_detection

**What it tests**:

- Server starts with IPSec stack
- Sends IKEv2 IKE_SA_INIT packet
- Verifies handshake detected in logs

**Packet structure**: 28-byte header + ~88 bytes payloads = ~116 bytes total

**Assertions**:

```rust
assert_stack_name(&mut server, "IPSEC");
assert!(output_str.contains("IPSec") || output_str.contains("IKE") || output_str.contains("handshake"));
```

**Expected output**:

```
[INFO] Starting IPSec/IKEv2 honeypot on 0.0.0.0:XXXXX (reconnaissance detection only)
[TRACE] IPSec: IKEv2 IKE_SA_INIT from 127.0.0.1:XXXXX (116 bytes)
[INFO] IPSec: IKEv2 handshake reconnaissance from 127.0.0.1:XXXXX
```

### 2. test_ipsec_ikev2_auth_detection

**What it tests**:

- Sends IKEv2 IKE_AUTH packet (exchange type 35)
- Verifies AUTH exchange detected

**Key difference**: Non-zero responder SPI (after SA_INIT completed)

**Expected output**:

```
[TRACE] IPSec: IKEv2 IKE_AUTH from 127.0.0.1:XXXXX (XXX bytes)
[INFO] IPSec: IKEv2 handshake reconnaissance from 127.0.0.1:XXXXX
```

### 3. test_ipsec_ikev1_detection

**What it tests**:

- Sends IKEv1 Identity Protection packet
- Verifies IKEv1 vs IKEv2 distinction

**Packet structure**: Version byte = 0x10 (IKEv1)

**Expected output**:

```
[TRACE] IPSec: IKEv1 Identity Protection from 127.0.0.1:XXXXX
```

### 4. test_ipsec_multiple_exchange_types

**What it tests**:

- Sends 4 different IKEv2 exchange types:
    1. IKE_SA_INIT (34)
    2. IKE_AUTH (35)
    3. CREATE_CHILD_SA (36)
    4. INFORMATIONAL (37)
- Verifies all exchanges logged

**Expected behavior**: Server logs all exchange types without crashing.

**Expected output**:

```
[TRACE] IPSec: IKEv2 IKE_SA_INIT from 127.0.0.1:XXXXX
[TRACE] IPSec: IKEv2 IKE_AUTH from 127.0.0.1:XXXXX
[TRACE] IPSec: IKEv2 CREATE_CHILD_SA from 127.0.0.1:XXXXX
[DEBUG] IPSec: IKEv2 CREATE_CHILD_SA from 127.0.0.1:XXXXX (logged)
[TRACE] IPSec: IKEv2 INFORMATIONAL from 127.0.0.1:XXXXX
[DEBUG] IPSec: IKEv2 INFORMATIONAL from 127.0.0.1:XXXXX (logged)
```

### 5. test_ipsec_concurrent_connections

**What it tests**:

- Three concurrent clients send IKE_SA_INIT
- Verifies honeypot handles concurrent UDP packets

**Concurrency**: Uses tokio::spawn for parallel sends.

**Expected behavior**: All handshakes logged, no packet loss.

## Known Issues

### LLM Stack Selection

**Issue**: LLM sometimes selects generic UDP stack instead of IPSec stack, even with explicit "via ipsec" keyword.

**Why**: Protocol keywords may not be strongly weighted in LLM's stack selection logic.

**Symptom**: `assert_stack_name()` fails with "Expected IPSEC, got UDP"

**Workaround**: Tests explicitly use "via ipsec" and validate stack name.

**Future fix**: Improve the base-stack documentation the model chooses from —
`generate_base_stack_documentation` in `src/llm/actions/common.rs`. (There is no
src/protocol/base_stack.rs; name resolution itself is `ServerRegistry::resolve` in
`src/protocol/server_registry.rs`, and it is not what is going wrong here.)

### No IKE Negotiation Testing

**Issue**: Tests don't verify IKE negotiation (SA establishment).

**Why**: Full IPSec requires:

- IKE SA establishment
- IPSec SA (ESP) creation
- Kernel XFRM policy configuration
- ESP encryption/decryption

**Acceptable**: Honeypot only detects packets, doesn't establish SAs.

### No ESP Testing

**Issue**: Tests don't cover ESP (Encapsulating Security Payload) packets.

**Why**: ESP packets arrive after IKE completes, which honeypot doesn't do.

**Future**: If ESP detection added to honeypot, add ESP packet tests.

### Crypto Validation

**Issue**: Tests use fake SPIs and crypto payloads (not cryptographically valid).

**Why**: Honeypot doesn't validate crypto, just logs packets.

**Acceptable**: Reconnaissance detection doesn't need valid crypto.

## Running Tests

### Prerequisites

```bash
# Build release binary with all features
./cargo-isolated.sh build --release --all-features
```

### Run Tests

```bash
# Run IPSec E2E tests
./cargo-isolated.sh test --features ipsec --test server::ipsec::e2e_test

# Run with output
./cargo-isolated.sh test --features ipsec --test server::ipsec::e2e_test -- --nocapture

# Run specific test
./cargo-isolated.sh test --features ipsec --test server::ipsec::e2e_test -- test_ipsec_ikev2_sa_init_detection
```

### Expected Output

```
running 5 tests
test test_ipsec_ikev2_sa_init_detection ... ok
test test_ipsec_ikev2_auth_detection ... ok
test test_ipsec_ikev1_detection ... ok
test test_ipsec_multiple_exchange_types ... ok
test test_ipsec_concurrent_connections ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 23.12s
```

### Handling Failures

If tests fail due to stack selection:

```
thread 'test_ipsec_ikev2_sa_init_detection' panicked at tests/server/ipsec/e2e_test.rs:32:5:
assertion failed: Expected stack name to contain "IPSEC", but got "UDP"
```

**Debug**:

1. Check server output: `./cargo-isolated.sh test ... -- --nocapture`
2. Verify LLM saw "via ipsec" keyword
3. Try different prompt phrasing
4. Check if IPSec protocol is enabled in build

## Use Cases

### Security Research

Tests demonstrate honeypot's ability to:

- Detect IPSec/IKE reconnaissance
- Log handshake attempts
- Identify IKE version (v1 vs v2)
- Track exchange types
- Extract SPIs

### Protocol Analysis

Tests verify:

- IKE header parsing works correctly
- Version detection (IKEv1 vs IKEv2)
- Exchange type identification
- SPI extraction
- Concurrent packet handling

## Future Improvements

### Full IPSec Server (Not Planned)

If full IPSec server ever implemented, tests would need:

```rust
#[tokio::test]
#[ignore] // Requires XFRM and root
async fn test_ipsec_full_tunnel() {
    // Requires IPSec library integration
    // Requires XFRM kernel support
    // Spawn real IPSec client (strongSwan, etc.)
    // Verify IKE negotiation
    // Test ESP tunnel traffic
}
```

**Note**: Full implementation is **not planned** - see IPSEC_RESEARCH.md for why.

### ESP Detection

Add ESP packet detection to honeypot:

```rust
#[tokio::test]
async fn test_esp_packet_detection() {
    let server = start_server("detect ESP packets on port 500").await;
    // Send ESP packet (IP protocol 50)
    // Verify ESP logged
}
```

### LLM Analysis Tests

Test LLM's ability to analyze IKE handshakes:

```rust
#[tokio::test]
async fn test_llm_ike_analysis() {
    let server = start_server("analyze and categorize IKE handshakes").await;
    // Send handshakes from different source IPs
    // Verify LLM identifies patterns (version, exchange types, SPIs)
}
```

### NAT-T Testing

Add NAT-T (UDP port 4500) tests:

```rust
#[tokio::test]
async fn test_ipsec_nat_t() {
    let server = start_server("start ipsec honeypot on port 4500").await;
    // Send NAT-T encapsulated IKE packets
    // Verify detection
}
```

## References

- [RFC 7296 - IKEv2](https://datatracker.ietf.org/doc/html/rfc7296)
- [RFC 2409 - IKEv1](https://datatracker.ietf.org/doc/html/rfc2409)
- [RFC 4301 - IPSec Architecture](https://datatracker.ietf.org/doc/html/rfc4301)
- [RFC 4303 - ESP](https://datatracker.ietf.org/doc/html/rfc4303)
- [NetGet IPSec Implementation](../../../src/server/ipsec/CLAUDE.md)
