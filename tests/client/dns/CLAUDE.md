# DNS Client E2E Test Strategy

## Overview

E2E tests for the DNS client protocol verify that NetGet can connect to DNS servers, send queries, and interpret
responses correctly through LLM-controlled actions.

## Test Approach

**Black-box testing**: Tests spawn the actual NetGet binary and verify behavior through stdout/stderr output and exit
codes.

**Target DNS Servers:**

- **Public DNS servers**: 8.8.8.8 (Google), 1.1.1.1 (Cloudflare)
- **Rationale**: DNS is a standard protocol, testing against public servers verifies real-world interoperability

**No local DNS server required**: Unlike other protocol tests, DNS tests use public infrastructure, simplifying test
setup.

## LLM Call Budget

**Total budget: < 5 LLM calls across all tests**

Breakdown:

- Test 1: A record query → 1-2 LLM calls (connect + query)
- Test 2: MX record query → 1-2 LLM calls (connect + query)
- Test 3: NXDOMAIN handling → 1-2 LLM calls (connect + query)
- Test 4: Multiple queries → 1-2 LLM calls (connect + multiple queries)

**Optimization strategies:**

1. Each test uses a single client instance
2. Simple, focused instructions minimize LLM decision complexity
3. No server startup needed (public DNS servers)
4. Tests run independently (no shared state)

## Test Cases

### Test 1: Basic A Record Query

**File:** `test_dns_client_a_record_query`
**LLM Calls:** 1-2
**Duration:** ~3 seconds

**Purpose:** Verify basic DNS client functionality with A record lookup.

**Instruction:**

```
Connect to 8.8.8.8:53 via DNS. Query A records for example.com and report the IP address.
```

**Expected Behavior:**

1. Client connects to 8.8.8.8:53
2. Sends DNS query for example.com A record
3. Receives response with IP address (93.184.216.34)
4. LLM reports the IP to user

**Assertions:**

- Output contains "DNS" or "dns"
- Client connects successfully
- No errors in output

### Test 2: MX Record Query

**File:** `test_dns_client_mx_record_query`
**LLM Calls:** 1-2
**Duration:** ~3 seconds

**Purpose:** Verify client can query different record types (MX for mail servers).

**Instruction:**

```
Connect to 1.1.1.1:53 via DNS. Query MX records for gmail.com and show the mail servers.
```

**Expected Behavior:**

1. Client connects to Cloudflare DNS (1.1.1.1:53)
2. Sends DNS query for gmail.com MX records
3. Receives response with multiple MX records and preferences
4. LLM reports mail servers

**Assertions:**

- Protocol is "DNS"
- Client connects successfully
- No errors in output

### Test 3: NXDOMAIN Handling

**File:** `test_dns_client_nxdomain`
**LLM Calls:** 1-2
**Duration:** ~3 seconds

**Purpose:** Verify client handles non-existent domains gracefully (NXDOMAIN response).

**Instruction:**

```
Connect to 8.8.8.8:53 via DNS. Query A records for nonexistent-domain-12345-xyz.com and report the result.
```

**Expected Behavior:**

1. Client connects to 8.8.8.8:53
2. Sends DNS query for nonexistent domain
3. Receives NXDOMAIN response
4. LLM handles gracefully and reports domain doesn't exist

**Assertions:**

- Output contains "DNS" or "dns"
- Client doesn't crash on NXDOMAIN
- No fatal errors

### Test 4: Multiple Queries

**File:** `test_dns_client_multiple_queries`
**LLM Calls:** 1-2
**Duration:** ~5 seconds

**Purpose:** Verify client can send multiple sequential queries in one session.

**Instruction:**

```
Connect to 8.8.8.8:53 via DNS. First query A records for google.com, then query AAAA records for google.com, and report both results.
```

**Expected Behavior:**

1. Client connects to 8.8.8.8:53
2. Sends first query for google.com A record
3. Receives response with IPv4 addresses
4. LLM sends follow-up query for AAAA records
5. Receives response with IPv6 addresses
6. LLM reports both IPv4 and IPv6 results

**Assertions:**

- Protocol is "DNS"
- Client executes both queries
- No errors between queries

## Running Tests

### Run all DNS client tests:

```bash
./cargo-isolated.sh test --no-default-features --features dns --test client::dns::e2e_test
```

### Run specific test:

```bash
./cargo-isolated.sh test --no-default-features --features dns --test client::dns::e2e_test test_dns_client_a_record_query
```

### With detailed output:

```bash
./cargo-isolated.sh test --no-default-features --features dns --test client::dns::e2e_test -- --nocapture
```

## Expected Runtime

- **Per test:** 3-5 seconds (mostly LLM processing time)
- **Total suite:** ~15 seconds (4 tests)
- **LLM overhead:** ~80-90% of test time
- **DNS query latency:** ~10-100ms per query

## Mock rule ordering (`test_dns_client_multiple_queries`)

`and_event_data_contains(key, value)` is a **substring** match
(`field.contains(value)` in `tests/helpers/mock_matcher.rs`), and the mock server
answers with the **first** rule that matches — `expect_calls(N)` does not retire a
rule once it is exhausted.

So a rule keyed on `query_type == "A"` also matches an `"AAAA"` response. With the
A rule declared first, the AAAA response was answered with "send another AAAA
query", and the client looped: 211 LLM calls, then a stack overflow
(`IMPROVEMENTS.md` item 49).

**The AAAA rule must be declared before the A rule.** `"A"` does not contain
`"AAAA"`, so the first response still falls through correctly. When adding record
types here, order the rules longest-value-first, or pick values that cannot be
substrings of one another.

The client side is now defended independently — the action loop is iterative rather
than recursive, and a per-client LLM call budget (default 100, see
`src/client/llm_budget.rs`) hard-stops any non-converging client — so a mistake of
this shape now fails the mock expectations instead of killing the process.

## Known Issues

### Issue 1: Public DNS Server Dependency

**Problem:** Tests depend on public DNS servers (8.8.8.8, 1.1.1.1) being accessible.
**Impact:** Tests may fail in isolated network environments.
**Mitigation:** Tests skip gracefully if DNS servers unreachable.
**Future:** Add local DNS server option (dnsmasq in Docker).

### Issue 2: Response Content Validation

**Problem:** Tests verify DNS protocol is used but don't validate exact response data.
**Reason:** DNS responses can vary (TTL, order, caching).
**Mitigation:** Tests focus on protocol behavior, not specific IP addresses.
**Alternative:** Add test with controlled local DNS server for deterministic responses.

### Issue 3: LLM Interpretation Variability

**Problem:** LLM may format responses differently between runs.
**Impact:** Hard to assert on exact output strings.
**Mitigation:** Tests verify protocol and connection, not exact output format.

### Issue 4: Timeout Handling

**Problem:** DNS queries have 5-second default timeout (hickory-client).
**Impact:** NXDOMAIN or slow servers may take several seconds.
**Mitigation:** Tests allow sufficient time (3-5 seconds per query).

## Test Infrastructure

### Helper Functions (in `tests/helpers/`)

- `start_netget_client()` - Spawn NetGet binary as client
- `NetGetConfig::new()` - Configure client instruction
- `output_contains()` - Check client output for expected content
- `stop()` - Clean shutdown of client process

### Test Environment

- **Ollama server**: Required for LLM processing
- **Model**: qwen3-coder:30b (or default model)
- **Isolation**: Each test runs in separate NetGet process

## Success Criteria

✅ All tests pass consistently
✅ < 5 LLM calls total across all tests
✅ Tests complete in < 20 seconds total
✅ No memory leaks or hung processes
✅ Graceful handling of errors (NXDOMAIN, timeouts)

## Debugging Tips

### Test hangs:

- Check Ollama server is running
- Verify network access to DNS servers (8.8.8.8, 1.1.1.1)
- Look for deadlocks in `netget.log`

### Test fails with connection errors:

- Verify DNS port 53 is not blocked by firewall
- Try alternative DNS server (1.1.1.1 instead of 8.8.8.8)
- Check if DNS resolution works: `dig @8.8.8.8 example.com`

### LLM makes incorrect decisions:

- Review LLM prompt in logs
- Check if instruction is clear and unambiguous
- Verify DNS actions are properly defined in protocol

### Flaky tests:

- Increase sleep duration after client startup
- Add retry logic for transient DNS failures
- Use local DNS server for deterministic behavior

## Future Enhancements

1. **Local DNS server**: Docker container with dnsmasq for deterministic responses
2. **DoT/DoH tests**: Test DNS-over-TLS and DNS-over-HTTPS clients
3. **DNSSEC validation**: Test DNSSEC-enabled queries
4. **Batch queries**: Test multiple concurrent queries
5. **Zone transfer**: Test AXFR/IXFR if supported
6. **Performance testing**: Measure queries per second with scripting mode
