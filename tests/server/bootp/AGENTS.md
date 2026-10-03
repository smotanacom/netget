# BOOTP E2E Test Documentation

## Test Strategy

BOOTP (Bootstrap Protocol) E2E tests validate the server's ability to handle BOOTREQUEST messages and respond with
correct BOOTREPLY packets containing IP assignments and boot file configuration.

### Testing Approach

- **Black-box testing**: Tests use real UDP client to send manually-crafted BOOTP packets
- **Packet construction**: Tests build BOOTREQUEST packets according to RFC 951
- **Response validation**: Parse BOOTREPLY packets and verify:
    - Operation code (op=2 for BOOTREPLY)
    - Transaction ID matches request
    - Assigned IP address (yiaddr field)
    - Server IP address (siaddr field)
    - Boot file name (file field, 128 bytes)
    - Server hostname (sname field, 64 bytes)
    - Client MAC echoed correctly

### Test Coverage

1. **Basic flow** - Simple BOOTREQUEST → BOOTREPLY exchange
2. **Boot file configuration** - Verify boot file path and server hostname in response
3. **Static assignment** - MAC-based static IP allocation

## LLM Call Budget

### Target: < 10 LLM calls per test suite

Current budget:

- **test_bootp_basic_flow**: 1 LLM call (server startup)
- **test_bootp_boot_file**: 1 LLM call (server startup)
- **test_bootp_static_assignment**: 1 LLM call (server startup)

**Total**: 3 LLM calls for full test suite ✓

### Optimization Techniques

1. **Single server per test** - One comprehensive instruction instead of multiple servers
2. **Manual packet construction** - No client library needed, direct UDP socket
3. **No scripting mode** - Tests validate LLM decision-making for each request
4. **Consolidated assertions** - Each test validates multiple aspects of response

## Runtime Characteristics

### Expected Runtime

- **Without Ollama cache**: 15-20 seconds per test (~45-60s total)
- **With Ollama cache**: 5-10 seconds per test (~15-30s total)
- **LLM processing**: 2-5 seconds per BOOTP request

### Breakdown

- Server startup: 2-3 seconds (LLM generates instruction understanding)
- BOOTREQUEST → BOOTREPLY: 2-5 seconds (LLM processes request, generates response)
- Packet construction: < 1ms
- Network I/O: < 1ms (localhost UDP)

### Performance Notes

- BOOTP is stateless UDP, so tests are fast once LLM responds
- dhcproto parsing/encoding: ~50-100 microseconds (negligible)
- Most time spent waiting for LLM to process BOOTREQUEST and generate BOOTREPLY
- Could enable scripting mode for faster tests (0 LLM calls after startup)

## Test Environment

### Requirements

- **Ollama**: Running with default model (qwen3-coder:30b or configured model)
- **Feature flag**: `bootp` must be enabled
- **Network**: Localhost UDP (no external network needed)
- **Privileges**: May require sudo for port 67 (can use high port for testing)

### Running Tests

```bash
# Build release binary first (faster startup)
./cargo-isolated.sh build --release --no-default-features --features bootp

# Run BOOTP E2E tests
./cargo-isolated.sh test --no-default-features --features bootp --test e2e_test -- --ignored

# Run specific test
./cargo-isolated.sh test --no-default-features --features bootp --test e2e_test test_bootp_basic_flow -- --ignored
```

## Known Issues

### 1. Port Binding

- Port 67 (BOOTP server port) is privileged
- Tests use `{AVAILABLE_PORT}` placeholder for dynamic port assignment
- Client binds to ephemeral port (not 68) for testing

### 2. Broadcast Handling

- BOOTP uses broadcast (255.255.255.255) in production
- Tests use direct localhost addressing for simplicity
- Broadcast flag (0x8000) set in requests for protocol compliance

### 3. Packet Format Assumptions

- Tests assume dhcproto handles BOOTP/DHCP magic cookie (99.130.83.99)
- Vendor-specific area (vend field) padded with zeros
- sname and file fields null-terminated C strings (64 and 128 bytes)

### 4. LLM Response Variations

- LLM may assign different IPs than expected (e.g., 192.168.1.101 instead of .100)
- Boot file path may vary (e.g., "pxeboot.n12" vs "boot/pxeboot.n12")
- Tests should accept reasonable variations or provide more specific instructions

## Test Data

### BOOTP Packet Structure (RFC 951)

```
Offset  Size  Field       Description
------  ----  ----------  -----------
0       1     op          Operation code (1=BOOTREQUEST, 2=BOOTREPLY)
1       1     htype       Hardware type (1=Ethernet)
2       1     hlen        Hardware address length (6 for MAC)
3       1     hops        Relay hops (0 for direct)
4       4     xid         Transaction ID (random)
8       2     secs        Seconds since client started
10      2     flags       Flags (0x8000=broadcast)
12      4     ciaddr      Client IP address (if known)
16      4     yiaddr      Your (client) IP address (assigned by server)
20      4     siaddr      Server IP address (next server to use)
24      4     giaddr      Gateway/relay IP address
28      16    chaddr      Client hardware address (MAC)
44      64    sname       Server host name (null-terminated)
108     128   file        Boot file name (null-terminated)
236     64    vend        Vendor-specific area (legacy)
```

### Example Test Instructions

#### Basic Server

```
BOOTP server that assigns IP addresses from 192.168.1.100 onwards.
When receiving BOOTREQUEST:
  - Assign the next available IP starting from 192.168.1.100
  - Use server IP: 192.168.1.1
  - Boot file: "boot/pxeboot.n12"
  - Server hostname: "bootserver"
```

#### PXE Boot Server

```
BOOTP server for PXE boot.
When receiving BOOTREQUEST:
  - Assign IP 10.0.0.100
  - Server IP: 10.0.0.1
  - Boot file: "tftp/netboot.img"
  - Server hostname: "netboot.example.com"
```

#### Static MAC Mapping

```
BOOTP server with static MAC-to-IP mappings.
When receiving BOOTREQUEST:
  - If MAC is 00:11:22:33:44:55, assign IP 192.168.1.50 with boot file "linux/vmlinuz"
  - If MAC is 00:AA:BB:CC:DD:EE, assign IP 192.168.1.51 with boot file "windows/bootmgr.efi"
  - For any other MAC, assign IP from 192.168.1.100 onwards with boot file "boot/default.pxe"
Use server IP 192.168.1.1 for all responses.
```

## Debugging

### Common Failures

#### 1. No Response

- **Symptom**: Test times out waiting for BOOTREPLY
- **Causes**:
    - LLM didn't understand BOOTP instruction
    - Server crashed during request processing
    - Wrong port/address
- **Debug**: Check netget.log for BOOTP receive/send messages

#### 2. Wrong IP Assignment

- **Symptom**: yiaddr doesn't match expected value
- **Causes**:
    - LLM interpreted instruction differently
    - LLM used different IP allocation strategy
- **Debug**: Check LLM response in logs, adjust instruction to be more specific

#### 3. Boot File Not Set

- **Symptom**: file field is empty or wrong
- **Causes**:
    - LLM didn't include boot_file parameter in action
    - Boot file path truncated (max 127 chars)
- **Debug**: Check LLM action JSON in logs

#### 4. Transaction ID Mismatch

- **Symptom**: xid in response doesn't match request
- **Causes**:
    - Bug in server implementation
    - Context not preserved correctly
- **Debug**: Check BootpRequestContext in mod.rs

### Log Levels

- **TRACE**: Full hex dump of BOOTREQUEST and BOOTREPLY packets
- **DEBUG**: Summary ("BOOTP received 300 bytes from 127.0.0.1")
- **INFO**: LLM decision messages ("Assigned 192.168.1.100 to client...")

### Manual Testing

```bash
# Start server manually
netget

# In another terminal, send BOOTP request
echo -ne '\x01\x01\x06\x00...' | nc -u localhost 67

# Or use a BOOTP client library (Python, etc.)
```

## Future Enhancements

### Potential Test Additions

1. **BOOTP relay test** - Validate giaddr (gateway/relay) handling
2. **Multiple requests test** - Verify IP pool management
3. **Malformed packet test** - Test error handling for invalid BOOTP packets
4. **Scripting mode test** - Enable scripting and verify 0 LLM calls after startup

### Test Optimizations

1. **Reuse server** - One server instance for all tests (reduces to 1 LLM call total)
2. **Parallel execution** - Run tests concurrently
3. **Mock LLM mode** - Bypass Ollama for faster CI tests (not currently implemented)

## References

- [RFC 951: Bootstrap Protocol (BOOTP)](https://datatracker.ietf.org/doc/html/rfc951)
- [RFC 1542: BOOTP Extensions](https://datatracker.ietf.org/doc/html/rfc1542)
- [dhcproto crate](https://docs.rs/dhcproto/)
- [PXE Specification](https://www.intel.com/content/www/us/en/architecture-and-technology/preboot-execution-environment.html)
