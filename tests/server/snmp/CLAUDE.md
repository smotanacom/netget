# SNMP Protocol E2E Tests

## Test Overview

Tests SNMP agent implementation with GET, GETNEXT, and custom MIB requests. Validates that the LLM can respond to SNMP
queries with proper OID values and data types. Tests both basic system OIDs and custom enterprise OIDs.

## Test Strategy

- **Isolated test servers**: Each test spawns a separate NetGet instance with specific OID
  instructions
- **Net-SNMP command-line tools only**: `snmpget` and `snmpgetnext`. **The `snmp` Rust crate is
  not used by any test in this directory**, despite what earlier revisions of this file and of
  `metadata().e2e_testing` said. The crate is a dependency of the SNMP *client* protocol, not of
  these tests
- **Every invocation passes `-On -Oe`**: numeric OIDs and numeric enums. Without them the
  rendering depends on which MIB files are installed — `ifOperStatus` prints `INTEGER: up(1)` on a
  machine with IF-MIB and `INTEGER: 1` on one without, and `sysDescr` prints
  `SNMPv2-MIB::sysDescr.0` rather than the numeric OID. Both variants were observed here
- **Assertions pin the type tag, not just the value**: `Gauge32: 1000000000`, `Counter32: 42`,
  `STRING: eth0`. A Gauge32 emitted with the INTEGER tag decodes to the same number and would pass
  a value-only check, while a real monitoring system would treat it as a different kind of
  quantity — and the BER encoder here is hand-rolled, so that is exactly the mistake worth
  catching
- **No fixed sleeps**: `spawn_with_llm_actions` binds the UDP socket before logging
  "SNMP agent … listening on", which is what `start_netget_server` waits for, so the socket is
  already accepting datagrams when the test resumes. The five 500 ms sleeps this suite carried
  (two of them duplicated back to back in `test_snmp_basic_get`) waited for something that had
  already happened

### `ber_depth_test.rs`

Four unit tests over `SnmpServer::parse_snmp_message`, no server and no mock model. They cover the
input screen described in `src/server/snmp/CLAUDE.md` §7: a 30000-level indefinite-length nesting
(which aborted the whole test binary with `stack overflow` before the screen existed), the
definite-length route to the same recursion, a length reaching past the end of the datagram, and a
real net-snmp `GetRequest` that must still parse.

## LLM Call Budget

- `test_snmp_basic_get()`: 2 LLM calls (sysDescr query + sysName query)
- `test_snmp_get_next()`: 1 LLM call (GETNEXT request)
- `test_snmp_interface_stats()`: 4 LLM calls (ifIndex, ifDescr, ifSpeed, ifOperStatus queries)
- `test_snmp_custom_mib()`: 3 LLM calls (3 custom enterprise OID queries)
- **Total: 10 LLM calls** (exactly at limit)

**Why At Limit?**:

1. SNMP is stateless - each request is independent
2. Each OID query requires separate LLM call (no batching)
3. Tests validate multiple scenarios (system OIDs, interface OIDs, custom OIDs)

**Optimization Opportunity**: Could use scripting mode to handle all queries with single setup call + 0 runtime calls.
However, tests currently use action-based mode to validate LLM's ability to interpret SNMP requests.

## Scripting Usage

❌ **Scripting Disabled** - Action-based responses only

**Rationale**: SNMP tests validate that LLM can:

1. Parse SNMP request format (OIDs, request type)
2. Generate appropriate responses (correct data types, values)
3. Handle different OID patterns (system, interface, custom)

Scripting would bypass this validation. However, scripting would dramatically improve performance (10 LLM calls → 1
setup call).

**Future Enhancement**: Add scripted SNMP test to validate script generation for OID responses. Would be useful for
high-traffic SNMP monitoring scenarios.

## Client Library

- **snmpget command-line tool** (from Net-SNMP package)
    - Most reliable SNMP client (used by sysadmins worldwide)
    - Handles all SNMP versions and PDU types
    - Used for basic GET tests

**Why the command-line tools?**

1. Net-SNMP is the reference implementation, and an *independent* one — which is what the Beta
   rating requires. A Rust crate that happened to share a codec with the server would be circular
   evidence
2. Easy to debug: the same command can be run by hand
3. The exact tool used in production SNMP monitoring

**`snmpgetnext` is what tests GETNEXT** — the `snmp` crate's `SyncSession` appears nowhere in this
directory.

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~60-90 seconds for full test suite (4 tests)
- Breakdown:
    - `test_snmp_basic_get()`: ~15-20s (2 LLM calls)
    - `test_snmp_get_next()`: ~10-15s (1 LLM call)
    - `test_snmp_interface_stats()`: ~25-30s (4 LLM calls)
    - `test_snmp_custom_mib()`: ~20-25s (3 LLM calls)

**Why Fast?**:

1. UDP protocol (no connection overhead)
2. Simple BER encoding/decoding (<1ms)
3. No authentication handshake
4. Each request independent (parallel processing possible)

## Failure Rate

- **Low** (~5%) - SNMP is simpler than TCP protocols
- Common failures:
    - LLM returns wrong data type (e.g., string instead of integer)
    - LLM doesn't understand OID notation (e.g., returns "1.3.6.1" as string)
    - Response timeout (LLM too slow or Ollama overload)
- Rare failures:
    - BER encoding error (server bug, not LLM issue)
    - Wrong OID returned (LLM misunderstands "next" in GETNEXT)

## Test Cases

### 1. SNMP Basic GET (`test_snmp_basic_get`)

- **LLM Calls**: 2 (sysDescr + sysName)
- **Prompt**: Respond to two OIDs - sysDescr and sysName
- **Client**: snmpget command-line tool (2 separate queries)
- **Queries**:
    1. `1.3.6.1.2.1.1.1.0` (sysDescr) - Expected: "NetGet SNMP Server v1.0"
    2. `1.3.6.1.2.1.1.5.0` (sysName) - Expected: "netget.local"
- **Validation**:
    - Response contains "NetGet" or "Server" (lenient check for sysDescr)
    - sysName query succeeds (any response is valid)
- **Purpose**: Basic SNMP GET operation with common system OIDs

### 2. SNMP GETNEXT (`test_snmp_get_next`)

- **LLM Calls**: 1 (GETNEXT request)
- **Prompt**: Support GETNEXT - when queried with 1.3.6.1.2.1.1, return next OID 1.3.6.1.2.1.1.1.0
- **Client**: snmp crate `SyncSession::getnext()`
- **Query**: `1.3.6.1.2.1.1` (base system OID)
- **Expected**: Next OID in tree (1.3.6.1.2.1.1.1.0 or similar)
- **Validation**: Response contains OID and value (any value is valid)
- **Purpose**: Test GETNEXT operation (table walking primitive)

**Note**: GETNEXT is tricky for LLM:

- Must understand lexicographic OID ordering
- Must return "next" OID in tree (not same OID)
- Common mistake: LLM returns same OID or wrong next OID

### 3. SNMP Interface Statistics (`test_snmp_interface_stats`)

- **LLM Calls**: 4 (ifIndex, ifDescr, ifSpeed, ifOperStatus)
- **Prompt**: Provide interface statistics for eth0 (OID 1.3.6.1.2.1.2.2.1.x.1)
- **Client**: snmp crate `SyncSession::get()`
- **Queries**:
    1. `1.3.6.1.2.1.2.2.1.1.1` (ifIndex) - Expected: 1 (integer)
    2. `1.3.6.1.2.1.2.2.1.2.1` (ifDescr) - Expected: "eth0" (string)
    3. `1.3.6.1.2.1.2.2.1.5.1` (ifSpeed) - Expected: 1000000000 (gauge/integer)
    4. `1.3.6.1.2.1.2.2.1.8.1` (ifOperStatus) - Expected: 1 (integer, up)
- **Validation**: All queries return responses (values checked if available)
- **Purpose**: Test interface MIB table queries (common SNMP use case)

**Note**: This tests SNMP table structure:

- Table OID: 1.3.6.1.2.1.2.2 (ifTable)
- Entry OID: 1.3.6.1.2.1.2.2.1 (ifEntry)
- Column OIDs: 1.3.6.1.2.1.2.2.1.X.N (X=column, N=interface index)

### 4. Custom Enterprise MIB (`test_snmp_custom_mib`)

- **LLM Calls**: 3 (3 custom OID queries)
- **Prompt**: Support custom enterprise OID tree 1.3.6.1.4.1.99999.x.x.0
- **Client**: snmp crate `SyncSession::get()`
- **Queries**:
    1. `1.3.6.1.4.1.99999.1.1.0` - Expected: "Custom Application v1.0" (string)
    2. `1.3.6.1.4.1.99999.1.2.0` - Expected: 42 (counter)
    3. `1.3.6.1.4.1.99999.1.3.0` - Expected: "active" (string)
- **Validation**: All queries return responses (values checked if available)
- **Purpose**: Test custom enterprise MIB support (non-standard OIDs)

**Note**: Enterprise OIDs:

- Format: 1.3.6.1.4.1.{enterprise}.{custom}
- 1.3.6.1.4.1 = iso.org.dod.internet.private.enterprises
- Each company/organization gets unique enterprise number
- 99999 is fictitious (used for testing)

## Known Issues

### 1. Net-SNMP is a hard requirement, not an optional one

**Symptom**: Test fails with "snmpget command not found"

**Cause**: Net-SNMP not installed on the test system

**Fix**: install it —

- macOS: ships with the system (`/usr/bin/snmpget`); otherwise `brew install net-snmp`
- Ubuntu: `apt-get install snmp`
- Fedora: `dnf install net-snmp-utils`

**Status**: the test **fails**; it does not skip. This file previously claimed "skips test if tool
not available", which was never true of the code — `Command::new("snmpget").output().await?`
propagates `NotFound` — and would have been the wrong design anyway. A skip-when-missing gate is a
silent pass, and SNMP's Beta rating rests entirely on these tests, so a runner without Net-SNMP
must say so loudly rather than report green.

### 2. LLM Data Type Confusion

**Symptom**: snmpget shows wrong data type (e.g., "INTEGER: Custom Application v1.0")

**Cause**: LLM returns wrong "type" in JSON (e.g., "integer" instead of "string")

**Frequency**: ~5% of tests, when driven by a real model rather than the mock

**Note**: the tests are **no longer lenient**. They assert the exact `TYPE: value` net-snmp
decoded, so a model that returns the wrong type now fails the test instead of being tolerated.

### 3. OID Notation Confusion

**Symptom**: LLM returns OID as string value instead of OID reference

**Cause**: LLM doesn't understand distinction between OID (object identifier) and value

**Example**:

- Prompt: "For OID 1.3.6.1.2.1.1.1.0, return 'System'"
- LLM: `{"oid": "1.3.6.1.2.1.1.1.0", "type": "string", "value": "1.3.6.1.2.1.1.1.0"}`
- Expected: `{"oid": "1.3.6.1.2.1.1.1.0", "type": "string", "value": "System"}`

**Frequency**: Rare (<1%)

### 4. GETNEXT Semantic Confusion

**Symptom**: GETNEXT returns same OID or wrong next OID

**Cause**: LLM doesn't understand lexicographic OID ordering

**Example**:

- Query: GETNEXT 1.3.6.1.2.1.1
- Expected: 1.3.6.1.2.1.1.1.0 (next OID)
- LLM Returns: 1.3.6.1.2.1.1 (same OID) or 1.3.6.1.2.1.2.0 (wrong next)

**Frequency**: ~10% of GETNEXT tests, when driven by a real model

**Note**: `test_snmp_get_next` now asserts that the answer names `1.3.6.1.2.1.1.1.0` — the OID
*after* the one queried — and carries the model's value. It previously printed the response and
checked nothing, so returning the queried OID itself would have passed.

## Performance Notes

### Why SNMP Is Fast

1. **UDP overhead**: ~100 bytes per request (vs TCP 3-way handshake)
2. **Stateless**: No connection setup, no teardown
3. **Simple encoding**: BER is compact (<100 bytes typical)
4. **No encryption**: SNMPv1/v2c is plaintext (fast)

### Bottleneck

- **LLM processing**: 2-5s per request (dominates)
- UDP + BER: <1ms total (negligible)

### Parallel Processing

Tests could run in parallel:

- Each query is independent
- No shared state between queries
- Ollama lock allows concurrent test execution

Example: 4 tests × 10 queries = 40 LLM calls

- Sequential: 40 × 5s = 200s (3.3 minutes)
- Parallel (4 concurrent): 10 × 5s = 50s (< 1 minute)

However, tests currently run sequentially for clarity.

### Scripting Mode Performance Improvement

If scripting were enabled:

**Current (action-based)**:

- Setup: 1 LLM call (~5s)
- 10 queries: 10 LLM calls (~50s)
- Total: ~55s

**With scripting**:

- Setup: 1 LLM call (~5s, generates script)
- 10 queries: 0 LLM calls (script handles, <1ms each)
- Total: ~5s

**Improvement**: 11x faster (55s → 5s)

## Future Enhancements

### Test Coverage Gaps

0. **Nothing runs these tests in CI.** The blocking `test` job compiles
   `tcp,http,dns,udp,redis,mcp-stdio`; `registry-audit` is `continue-on-error` and does not run
   e2e suites. SNMP's Beta rating is therefore only ever re-checked by someone running the suite
   locally.
1. **SET requests**: No tests for SetRequest (write operations)
2. **GETBULK**: No tests for GetBulkRequest (SNMPv2c bulk retrieval)
3. **Traps**: No tests for SNMP traps (proactive notifications)
4. **Table walking**: No tests for iterative GETNEXT (walking entire table)
5. **Error responses**: No tests for noSuchName, genErr error responses
6. **Community string validation**: No tests for wrong community string

### Consolidation Opportunity

All tests could be consolidated into single comprehensive server:

```rust
let prompt = format!(
    "listen on port {} via snmp.
    System OIDs:
    - 1.3.6.1.2.1.1.1.0: 'NetGet SNMP Agent'
    - 1.3.6.1.2.1.1.5.0: 'netget.local'
    Interface eth0 (ifIndex=1):
    - 1.3.6.1.2.1.2.2.1.1.1: 1 (ifIndex)
    - 1.3.6.1.2.1.2.2.1.2.1: 'eth0' (ifDescr)
    - 1.3.6.1.2.1.2.2.1.5.1: 1000000000 (ifSpeed)
    - 1.3.6.1.2.1.2.2.1.8.1: 1 (ifOperStatus)
    Custom MIB (1.3.6.1.4.1.99999.1.x.0):
    - 1.3.6.1.4.1.99999.1.1.0: 'Custom App'
    - 1.3.6.1.4.1.99999.1.2.0: 42 (counter)
    - 1.3.6.1.4.1.99999.1.3.0: 'active'
    For GETNEXT 1.3.6.1.2.1.1, return 1.3.6.1.2.1.1.1.0",
    port
);

// Single server handles all test cases
// Would reduce from 4 servers to 1
// Savings: ~15-20s of test time (server startup overhead)
```

However, this loses test isolation - one LLM failure affects all validations.

### Scripting Mode Test

Add test for SNMP scripting:

```rust
#[tokio::test]
async fn test_snmp_scripted_responses() -> E2EResult<()> {
    let prompt = format!(
        "listen on port {} via snmp. Use script to handle all OID queries:
        - System OIDs return standard values
        - Unknown OIDs return noSuchName error",
        port
    );

    // Verify script was generated
    assert!(server.output_contains("script_inline").await);

    // Verify only 1 LLM call (setup, no calls for queries)
    assert_eq!(server.count_in_output("LLM request:").await, 1);

    // Send 10 SNMP queries
    for i in 0..10 {
        // All queries use script (0 LLM calls)
    }
}
```

This would validate that scripting works for SNMP and show 10x performance improvement.

## References

- [RFC 1157: SNMP (SNMPv1)](https://datatracker.ietf.org/doc/html/rfc1157)
- [RFC 3416: SNMPv2 Protocol Operations](https://datatracker.ietf.org/doc/html/rfc3416)
- [snmp crate Documentation](https://docs.rs/snmp/latest/snmp/)
- [Net-SNMP Tools](http://www.net-snmp.org/) - Command-line tools (snmpget, snmpwalk, etc.)
- [SNMP OID Reference](http://www.oid-info.com/) - Lookup OID meanings
- [MIB-II (RFC 1213)](https://datatracker.ietf.org/doc/html/rfc1213) - Standard system/interface OIDs
