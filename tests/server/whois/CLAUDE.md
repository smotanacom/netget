# WHOIS E2E Testing

## Test Strategy

`tests/server/whois/e2e_test.rs`. Five tests, **6 LLM calls total**.

**`test_whois_with_real_whois_client` is what the `Beta` rating rests on**: it runs the real
`whois(1)` binary and asserts the record it prints. A raw socket only proves bytes arrived; a
real client exiting 0 with the record on stdout proves the framing, the CRLF line endings and
the close are all acceptable to it. The remaining four tests use a raw `TcpStream` for the
cases `whois(1)` cannot reach — it sends exactly one query and then reads to EOF, so an error
reply, two queries on one connection, and connection logging all need a socket.

Two facts about `whois(1)` to know before touching these tests:

- macOS's `whois` **segfaults** when `-h` is given an **IP literal** (`-h 127.0.0.1`) — it does
  so against a plain `nc` listener too, so it is a client bug. `-h localhost` works, and still
  resolves to loopback only. An earlier note in `src/server/whois/CLAUDE.md` read this as "the
  real client is unusable"; it is not.
- It reads until EOF. RFC 3912 has the server close as soon as its output is finished, but this
  server keeps the connection open, so **the handler must answer with `close_connection`** or
  `whois(1)` blocks forever. The real-client test pairs `send_whois_record` with it, and both
  `send_*` action descriptions now say so.

The test hard-fails if `whois` is not installed, matching what the SNMP suite does with
`snmpget`. `whois` is not in the CI feature set, so this does not gate PRs.

## LLM Call Budget

One startup call plus one per query:

| Test | Calls |
|---|---|
| `test_whois_with_real_whois_client` | 1 + 1 = 2 |
| `test_whois_basic_query` | 1 + 1 = 2 |
| `test_whois_error_response` | 1 + 1 = 2 |
| `test_whois_multiple_queries` | 1 + 2 = 3 |
| `test_whois_connection_stats` | 1 + 1 = 2 |

## Runtime

**Estimated**: ~30-45 seconds

- Server startup: ~5-10s
- Each test case: ~5-8s per LLM call
- Cleanup: minimal

## Test Plan

### Test 1: Basic Domain Query

**Setup**: Server with instruction to respond with fake data
**Action**: Send `example.com\r\n`
**Validation**:

- Response contains "Domain Name: example.com"
- Response contains registrar information
- Response contains nameservers

### Test 2: Error Response

**Setup**: Same server
**Action**: Send `nonexistent-domain-xyz123.com\r\n`
**Validation**:

- Response contains "Error" or "not found"
- Connection remains open or closes gracefully

### Test 3: Multiple Queries on Same Connection

**Setup**: Server that keeps connections open
**Action**: Send multiple queries sequentially
**Validation**:

- All queries receive responses
- Connection stays open between queries
- Stats track multiple packets

### Test 4: Connection Close

**Setup**: Server instructed to close after first response
**Action**: Send query
**Validation**:

- Receive response
- Connection closes (EOF)
- Connection status updated to Closed

## Implementation Notes

### Efficient Testing

Reuse server instance across test cases by using comprehensive initial prompt:

```
WHOIS server on port 43
For example.com: respond with registrar "Test Registrar", registrant "Test Org", nameservers ns1/ns2.example.com
For unknown domains: return "Domain not found" error
Keep connections open for multiple queries
```

This allows testing multiple scenarios with a single server startup.

### Privacy Requirements

- All tests use `127.0.0.1` (localhost only)
- No external network access
- No real WHOIS queries
- Works completely offline

### Ollama Lock

Tests use an in-process mock LLM, so concurrent runs share no model server.

## Known Issues

None currently identified.

## Test Execution

```bash
./cargo-isolated.sh test --no-default-features --features whois \
    --test server -- --test-threads=100 whois
```

## Not covered

Referrals to another WHOIS server, WHOIS++ (RFC 1835), IDN, and non-ASCII queries. The server
implements none of them.
