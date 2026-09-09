# DNS Protocol E2E Tests

## Test Overview

Tests DNS server implementation with A, TXT, and multiple record queries. Validates NXDOMAIN handling and multi-domain
resolution. Uses hickory-client (real DNS client) for protocol correctness.

## Test Strategy

- **Isolated test servers**: Each test spawns separate NetGet instance with specific DNS configuration
- **Real DNS client**: Uses hickory-client library (AsyncClient + UdpClientStream)
- **Protocol correctness**: Tests actual DNS wire protocol, not mocked responses
- **Record type coverage**: Tests A, TXT records; NXDOMAIN response
- **No scripting**: Action-based LLM responses (tests LLM's ability to handle DNS semantics)
- **Dynamic mocks**: Uses `.respond_with_actions_from_event()` for protocol-correct transaction ID matching

## LLM Call Budget

- `test_dns_a_record_query()`: 1 LLM call (A record query)
- `test_dns_multiple_records()`: 2 LLM calls (example.com + mail.example.com queries)
- `test_dns_txt_record()`: 1 LLM call (TXT record query)
- `test_dns_nxdomain()`: 1 LLM call (NXDOMAIN for unknown domain)
- `dig_test::test_dns_answers_dig()`: 1 startup + 3 query calls, one server
- **Total: 9 LLM calls** (under the 10 limit; `dig_test` bundles its three cases into
  one server rather than spawning three, which is what keeps it there)

**Optimization Opportunity**: Could consolidate into single comprehensive DNS server handling all record types and
domains, reducing to 1 startup call + 5 query calls = 6 total. However, current approach provides better isolation and
clearer failure diagnosis.

## Scripting Usage

❌ **Scripting Disabled** - Action-based responses only

**Rationale**: Tests validate that LLM can correctly generate DNS responses using structured actions. Scripting would
bypass this validation. For production DNS servers, scripting is highly recommended for performance.

## Dynamic Mock Pattern (CRITICAL)

All DNS tests use **dynamic mocks** via `.respond_with_actions_from_event()` to enable protocol-correct transaction ID matching.

### Why Dynamic Mocks?

DNS (and all UDP protocols) require **transaction ID matching**: the `query_id` in the response must exactly match the `query_id` in the request. Static mocks cannot do this because the client generates random transaction IDs.

**Problem with static mocks:**
```rust
// ❌ WRONG - hardcoded query_id doesn't match request
.respond_with_actions(serde_json::json!([{
    "type": "send_dns_a_response",
    "query_id": 0,  // ← Static! Client expects 15073
    "domain": "example.com",
    "ip": "93.184.216.34"
}]))
```

**Solution with dynamic mocks:**
```rust
// ✅ CORRECT - extract query_id from event data
.respond_with_actions_from_event(|event_data| {
    let query_id = event_data["query_id"].as_u64().unwrap_or(0);
    serde_json::json!([{
        "type": "send_dns_a_response",
        "query_id": query_id,  // ← Dynamic! Matches request
        "domain": "example.com",
        "ip": "93.184.216.34"
    }])
})
```

### Pattern Usage

**Step 1**: Match the event type and event data:
```rust
.on_event("dns_query")
.and_event_data_contains("domain", "example.com")
.and_event_data_contains("query_type", "A")
```

**Step 2**: Use dynamic response with closure:
```rust
.respond_with_actions_from_event(|event_data| {
    // Extract dynamic values from event
    let query_id = event_data["query_id"].as_u64().unwrap_or(0);

    // Return actions using extracted values
    serde_json::json!([{
        "type": "send_dns_a_response",
        "query_id": query_id,
        "domain": "example.com",
        "ip": "93.184.216.34",
        "ttl": 300
    }])
})
```

**Step 3**: Set call expectations:
```rust
.expect_calls(1)
.and()
```

### Full Example

```rust
let config = NetGetConfig::new("listen on port {AVAILABLE_PORT} via dns. ...")
    .with_log_level("debug")
    .with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("dns")
            .respond_with_actions(serde_json::json!([
                {"type": "open_server", "port": 0, "base_stack": "DNS", ...}
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: DNS query event - DYNAMIC RESPONSE
            .on_event("dns_query")
            .and_event_data_contains("domain", "example.com")
            .and_event_data_contains("query_type", "A")
            .respond_with_actions_from_event(|event_data| {
                let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                serde_json::json!([{
                    "type": "send_dns_a_response",
                    "query_id": query_id,  // ← CRITICAL: Must match request!
                    "domain": "example.com",
                    "ip": "93.184.216.34"
                }])
            })
            .expect_calls(1)
            .and()
    });

let server = helpers::start_netget_server(config).await?;

// ... send DNS query ...

server.verify_mocks().await?;  // ← CRITICAL: Verify mock expectations
```

### Key Points

1. **Extract event data inside closure**: `event_data["query_id"]`, `event_data["domain"]`, etc.
2. **Return JSON array or single object**: Normalizes automatically
3. **Closure signature**: `Fn(&serde_json::Value) -> serde_json::Value`
4. **Always call `.verify_mocks().await?`**: Ensures expectations met
5. **Dynamic mocks are NOT serializable**: Requires in-process mock server (not env var)

### Event Data Available

For `dns_query` events:
- `query_id` (number) - Transaction ID from DNS request packet
- `domain` (string) - Domain name being queried
- `query_type` (string) - Record type (A, AAAA, TXT, MX, etc.)

### Transaction ID Matching Evidence

**Correct behavior with dynamic mocks:**
```
DNS request:  0x3ae1 (15073 decimal)
Mock extracts: query_id = 15073
DNS response: 0x3ae1 (matches!)
Client accepts response ✅
```

**Broken behavior with static mocks:**
```
DNS request:  0x3ae1 (15073 decimal)
Mock returns: query_id = 0
DNS response: 0x0000 (doesn't match!)
Client ignores response ❌ timeout
```

## Client Library

- **hickory-client v0.24** - Async DNS client library
    - `AsyncClient` - High-level DNS query interface
    - `UdpClientStream` - UDP transport for DNS queries
    - Handles DNS wire protocol automatically
    - Validates response format

**Why hickory-client?**:

1. Real DNS protocol validation (not just "any UDP response")
2. Async/await compatible with Tokio

Point 3 used to read "same library family as server-side hickory-proto", listed as an
advantage. It is the opposite: hickory-client decodes with the same codec
hickory-proto encoded with, so on its own this suite proves the wire format is
self-consistent, not that it is correct. That is the circularity that kept `rss` at
Experimental until `feed-rs` did its parsing.

**`dig_test.rs` is the answer to it.** ISC BIND's `dig` shares no code with hickory,
and it is stricter than the round-trip - it checks that the transaction id it chose
comes back and that the question section matches what it asked, so a reply a real
resolver would discard fails there and passes here. It **fails** when `dig` is absent
rather than printing SKIP, per the `npm` precedent.

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~40-50 seconds for full test suite (4 tests × ~10s each)
- Each test includes: server startup (2-3s) + LLM response (5-8s) + DNS query (<1s)

**Note**: DNS tests are faster than some other protocols because:

- UDP is connectionless (no TCP handshake)
- hickory-client is very fast
- No complex protocol state machine

## Failure Rate

- **Low** (~2-3%) - Occasional LLM response issues
- Most common failure: LLM returns wrong record type or malformed IP address
- Timeout failures: Very rare (<1%) - DNS queries have 5s timeout
- NXDOMAIN test: Sometimes flaky if LLM misinterprets "unknown domain" instruction

## Test Cases

### 1. DNS A Record Query (`test_dns_a_record_query`)

- **Prompt**: "listen on port {port} via dns. Respond to all A record queries for example.com with IP address
  93.184.216.34"
- **Client**: Queries example.com A record using hickory-client
- **Expected**: exactly one A record holding 93.184.216.34, RCODE NOERROR
- **Purpose**: Tests basic IPv4 address resolution
- **Validation**: `answer_a()` asserts one A record by value

### 2. DNS Multiple Records (`test_dns_multiple_records`)

- **Prompt**: "listen on port {port} via dns. For example.com A records return 1.2.3.4. For mail.example.com A records
  return 5.6.7.8"
- **Client**: Queries both example.com and mail.example.com
- **Expected**: Each query returns appropriate A record
- **Purpose**: Tests multi-domain configuration and LLM's ability to distinguish domains
- **LLM Calls**: 2 (one per domain query)

### 3. DNS TXT Record (`test_dns_txt_record`)

- **Prompt**: "listen on port {port} via dns. For TXT record queries on example.com, return 'v=spf1 include:_
  spf.example.com ~all'"
- **Client**: Queries example.com TXT record
- **Expected**: Response contains TXT record
- **Purpose**: Tests non-address record type (SPF record)
- **Validation**: Checks for TXT record in answers

### 4. DNS NXDOMAIN (`test_dns_nxdomain`)

- **Prompt**: "listen on port {port} via dns. Only respond with A records for known.example.com (1.2.3.4). For all other
  domains, return NXDOMAIN"
- **Client**: Queries unknown.example.com (should fail)
- **Expected**: RCODE 3 (NXDOMAIN), zero answer records, question echoed
- **Purpose**: Tests error handling and NXDOMAIN response
- **Note**: all three are asserted. See "Known Issues" for what this used to accept

## Known Issues

### 1. NXDOMAIN Test Variability - fixed, and it was not variability

`test_dns_nxdomain` used to match on `Ok`/`Err` and print
"implementation-dependent behavior" in one arm and "server indicated domain not found"
in the other, so **every** outcome passed - including NOERROR with an empty answer
section, which tells a resolver the opposite of NXDOMAIN. Nothing here is
implementation-dependent: the mock forces `send_dns_nxdomain`, so RCODE 3 is the only
correct answer, and the test now asserts the RCODE, the empty answer section and the
echoed question. `dig_test.rs` asserts `status: NXDOMAIN` off the header line for the
same reason - an empty answer section alone does not distinguish the two.

### 2. No Record Content Validation - fixed

Tests asserted `!answers.is_empty()`, which passes for an executor that ignores the
`ip` it was handed. They now assert the address, the TXT character-string and the
RCODE by value. The old rationale - "LLM might format responses slightly differently"
- does not apply: these are mock-driven, the handler's output is fixed, and the values
are compared after hickory has decoded them into typed rdata, not as text.

### 3. No AAAA, MX, CNAME Tests

Current test suite only covers A and TXT records. Other record types are supported by the protocol but not tested.

**Rationale**: A and TXT provide good coverage of address and text record types. Adding all record types would exceed
LLM call budget.

**Future Enhancement**: Create consolidated test with one server handling all record types.

### 4. No Concurrent Query Tests

Tests send queries sequentially. No validation of concurrent query handling.

**Reason**: DNS server handles concurrent queries correctly (separate tokio tasks per query), but testing concurrency
would complicate assertions and increase LLM calls.

## Performance Notes

### Why hickory-client?

Originally considered using raw UDP sockets (like DHCP/NTP tests), but hickory-client provides several advantages:

- Validates DNS wire protocol compliance
- Automatic query ID generation and matching
- Timeout handling built-in
- Parses responses into structured Record types
- Minimal overhead (~1ms per query)

### DNS Protocol Characteristics

DNS is inherently fast:

- UDP transport (no TCP handshake)
- Small packet sizes (typically <512 bytes)
- Stateless request-response
- No authentication/encryption overhead

Without LLM overhead, NetGet DNS server could handle thousands of queries per second with scripting enabled.

## Future Enhancements

### Test Coverage Gaps

1. **AAAA records**: No IPv6 testing
2. **MX records**: No mail exchange testing
3. **CNAME records**: No alias testing
4. **Multiple answers**: No testing of multiple A records for same domain
5. **SOA records**: No zone authority testing
6. **NS records**: No nameserver delegation testing
7. **Large responses**: No testing of 512-byte UDP limit
8. **Malformed queries**: No testing of invalid DNS packets

### Consolidation Opportunity

All four tests could be consolidated into a single comprehensive server:

```rust
let prompt = format!(
    "listen on port {} via dns.
    - For example.com A: return 1.2.3.4
    - For mail.example.com A: return 5.6.7.8
    - For example.com TXT: return 'v=spf1 mx ~all'
    - For known.example.com A: return 93.184.216.34
    - For all other domains: return NXDOMAIN",
    port
);
```

This would reduce from 4 server spawns to 1, saving ~8-12 seconds of test time and reducing LLM calls from 5 to 4 (1
startup + 4 queries).

### Scripting Mode Test

Add test with scripting enabled to validate script generation:

- Verify script handles A, TXT, NXDOMAIN correctly
- Measure throughput improvement (should be 1000x faster)
- Ensure script doesn't call LLM for each query

## References

- [RFC 1034: DNS Concepts](https://datatracker.ietf.org/doc/html/rfc1034)
- [RFC 1035: DNS Implementation](https://datatracker.ietf.org/doc/html/rfc1035)
- [hickory-client Documentation](https://docs.rs/hickory-client/latest/hickory_client/)
- [DNS Response Codes (IANA)](https://www.iana.org/assignments/dns-parameters/dns-parameters.xhtml#dns-parameters-6)
