# OSPF E2E Test Documentation

## Test Strategy

**Goal**: Verify OSPF server can establish neighbor relationships and exchange basic protocol packets with LLM control.

**Approach**: Black-box testing using manual OSPF client that sends Hello packets and validates responses.

**LLM Budget**: Target < 5 LLM calls

- 1 call for server startup (parse instruction)
- 2-3 calls for Hello packet exchanges
- 1 call for neighbor state verification

**Runtime**: ~30-40 seconds (mostly LLM response time)

## Test Scenarios

### 1. Server Startup

- Start OSPF server on port 2600
- LLM parses instruction: "Listen on port 2600 via OSPF as router 1.1.1.1 in area 0.0.0.0"
- Verify server binds to UDP socket

### 2. Hello Packet Exchange

- Client sends Hello packet to server
- LLM receives `ospf_hello` event with neighbor information
- LLM responds with `send_hello` action
- Verify Hello response received

### 3. Neighbor State Transition

- Exchange multiple Hello packets
- Verify neighbor state transitions: Down → Init → 2-Way
- Confirm bidirectional communication established

### 4. Database Description Exchange (Optional)

- Client sends Database Description packet
- LLM receives `ospf_database_description` event
- LLM responds with DD packet
- Verify DD response received

## Test Implementation

**Test file**: `e2e_test.rs`

**Setup**:

```rust
#[tokio::test]
#[cfg(all(test, feature = "ospf"))]
async fn test_ospf_hello_exchange() {
    // Start server with LLM
    // Create UDP client
    // Send Hello packet
    // Verify response
}
```

**Manual OSPF Client**:

- Construct OSPF Hello packet (24-byte header + Hello body)
- Send via UDP socket
- Parse response packets
- Verify packet structure and fields

## Known Issues

1. **No SPF Calculation**: Server doesn't calculate shortest paths (by design)
2. **DR/BDR Election**: Tracked but not enforced
3. **LSA Parsing**: Uses basic parsing, no full LSA database
4. **Authentication**: Not implemented
5. **Non-Standard Transport**: Uses UDP instead of IP protocol 89

## Success Criteria

- ✅ Server starts and binds to UDP port
- ✅ Server receives Hello packet
- ✅ LLM processes Hello event
- ✅ Server sends Hello response
- ✅ Neighbor state transitions correctly
- ✅ Total LLM calls < 5
- ✅ Test completes in < 60 seconds

## Running Tests

```bash
# Build release binary first
./cargo-isolated.sh build --release --no-default-features --features ospf

# Run E2E test
./cargo-isolated.sh test --no-default-features --features ospf --test server::ospf::e2e_test
```

## Debugging

**Enable logging**:

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features ospf --test server::ospf::e2e_test
```

**Check packet structure**:

- Verify OSPF header (24 bytes): version, type, length, router ID, area ID
- Verify Hello body: network mask, hello interval, priority, DR, BDR
- Check checksum calculation

## Future Improvements

1. Add Database Description exchange test
2. Test LSA flooding
3. Verify neighbor timeout/dead interval
4. Test multiple concurrent neighbors
5. Add DR/BDR election test

## References

- RFC 2328 - OSPFv2
- `src/server/ospf/CLAUDE.md` - Implementation details
- `tests/README.md` - Test framework documentation
