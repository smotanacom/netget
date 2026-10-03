# NTP Client Testing

## Test Strategy

Black-box E2E testing using public NTP servers. Tests verify NTP client can query time servers and interpret responses.

## Test Approach

### Public NTP Servers

- **time.google.com:123** - Google's public NTP (stratum 1, very reliable)
- **pool.ntp.org:123** - NTP pool (stratum 2-3, community servers)

### Why Public Servers?

1. **No Mock Needed:** NTP is a simple, stateless protocol with public infrastructure
2. **Real Behavior:** Tests against actual NTP implementations
3. **Network Reliability:** Google's NTP is highly available
4. **Stratum Diversity:** Pool provides different stratum levels for testing

## LLM Call Budget

Each test is designed to stay under **3 LLM calls**:

1. **`test_ntp_client_query_time_server`** - 2 calls
    - Call 1: Client startup (interprets instruction, triggers `query_time` action)
    - Call 2: Response processing (analyzes timestamps)

2. **`test_ntp_client_stratum_analysis`** - 2 calls
    - Call 1: Client startup
    - Call 2: Stratum interpretation

3. **`test_ntp_client_single_query_model`** - 2 calls
    - Call 1: Client startup
    - Call 2: Response processing
    - Validates single-query design (client disconnects after one query)

**Total LLM calls across all tests:** 6 calls (well under 10)

## Expected Runtime

- **Per test:** 6-7 seconds
    - 500ms: Client startup
    - 5000ms: NTP query timeout (actual response typically < 100ms)
    - 500ms: LLM processing

- **All tests:** ~20 seconds (3 tests × 7 seconds)

## Test Cases

### 1. Query Time Server

**Purpose:** Verify client can query NTP server and receive response

**Validation:**

- Output contains "ntp" or "time" keywords
- Client completes without errors

**LLM Instruction:** "Query time.google.com:123 for current time and show the server time."

**Expected LLM Behavior:**

- Triggers `query_time` action
- Receives `ntp_response_received` event with timestamps
- Displays server time or offset

### 2. Stratum Analysis

**Purpose:** Verify client can extract and report stratum level

**Validation:**

- Protocol is "NTP"
- Client processes response (no errors)

**LLM Instruction:** "Query pool.ntp.org:123 and report the stratum level."

**Expected LLM Behavior:**

- Triggers `query_time` action
- Receives `ntp_response_received` event with `stratum` field
- Reports stratum value (typically 2-3 for pool.ntp.org)

### 3. Single Query Model

**Purpose:** Validate single-query design (client disconnects after one query)

**Validation:**

- Client completes query
- Client terminates after single query (documented behavior)

**LLM Instruction:** "Query time.google.com:123 for the current time."

**Expected LLM Behavior:**

- Triggers `query_time` action
- Receives response
- Client disconnects (single-query model)

## Known Issues

### Network Dependency

- **Public Internet Required:** Tests fail without internet access
- **Firewall Issues:** UDP port 123 must be allowed outbound
- **DNS Resolution:** Requires working DNS to resolve time.google.com, pool.ntp.org

### Timeout Sensitivity

- **5 Second Timeout:** Fixed in client implementation
- **Network Delays:** Slow networks may approach timeout
- **Retry Not Implemented:** Single attempt only

### Response Variability

- **Stratum Values:** pool.ntp.org may return different stratum levels (2-3)
- **Server Selection:** Pool randomly selects from multiple servers
- **Timestamp Precision:** Varies by server (microsecond to millisecond)

## Flaky Test Mitigation

### Network Failures

- Use reliable public servers (Google NTP has 99.9%+ uptime)
- Accept timeout as expected failure mode (not flaky)
- Avoid assertions on exact timestamp values (check reasonableness only)

### Parallel Execution

- Tests are independent (no shared state)
- Each test uses different instruction (no interference)
- UDP is stateless (no port conflicts)

## Test Environment

### Requirements

- **Internet Access:** Yes (public NTP servers)
- **Ollama Running:** Yes (LLM calls required)
- **Feature Flag:** `--features ntp`
- **Ports:** UDP 123 outbound (ephemeral source port)

### Test Isolation

- **No Local Server:** Uses public NTP infrastructure
- **No State:** Each test is independent
- **No Cleanup:** UDP sockets automatically released

## Debugging Tips

### Test Failures

**"Client should show NTP response" assertion fails:**

- Check internet connectivity
- Verify UDP port 123 is not blocked by firewall
- Check Ollama is running and responsive
- Review client output for error messages

**Timeout (test hangs):**

- NTP server may be unreachable
- UDP packets may be dropped
- Firewall may be blocking NTP traffic
- Check `netstat -an | grep 123` for active connections

**Protocol mismatch:**

- Verify NTP feature is enabled (`--features ntp`)
- Check client_registry.rs includes NTP registration
- Confirm client/mod.rs exports NtpClientProtocol

### Manual Testing

```bash
# Test NTP server reachability
ntpdate -q time.google.com

# Or use netget directly
./cargo-isolated.sh run --no-default-features --features ntp -- \
  open_client ntp time.google.com:123 "Query time and show offset"
```

## Future Test Improvements

### Clock Offset Validation

- Parse LLM output to extract reported offset
- Verify offset is within reasonable bounds (< 1 hour)
- Compare with system clock

### Stratum Level Assertions

- Assert stratum is in valid range (1-15)
- Verify stratum 0 is rejected (invalid)
- Test different stratum levels (1, 2, 3)

### Precision Testing

- Verify precision field is negative (log2 seconds)
- Check precision is reasonable (-20 to -1)

### Multi-Query Testing

- Modify client to support multiple queries
- Test query multiple servers
- Calculate average offset across servers

### Error Handling

- Test unreachable server (expect timeout)
- Test invalid NTP response (malformed packet)
- Test DNS resolution failure
