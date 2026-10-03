# IMAP Client E2E Test Documentation

## Test Strategy

The IMAP client E2E tests use a black-box approach with the actual NetGet binary. Tests spawn both IMAP server and
client instances to verify LLM-controlled behavior.

## Test Approach

### Black-Box Testing

- **Binary execution:** Tests spawn `target/release/netget` in scripting mode
- **Real protocol:** Client connects to NetGet IMAP server (or real test server)
- **LLM integration:** Tests validate that LLM interprets prompts and executes actions
- **Mocked model, real protocol:** the LLM is `mock_ollama`; the IMAP exchange itself is a
  real one over TCP between two NetGet processes

### Every mock reply must echo the event's tag

`async-imap` numbers its own tags (`a1`, `a2`, …) and matches replies by tag. A mock that
hardcodes one — `"tag": "A001"`, or a raw `"A002 OK …"` in a `response` string — tags the
reply to a command the client never sent, so async-imap discards it as unsolicited and the
client parks until the test gives up. It then fails as *"the client never connected"*, which
points at the client rather than at the mock, and every rule after the stall reports
`expected 1, got 0`.

Use `.respond_with_actions_from_event(|e| …)` and take the tag from the event
(`e["tag"]`); both `imap_auth` and `imap_command` carry it. This is the same rule the DNS
tests document for query ids — anything the peer generates and matches on has to come back
out of the event.

Two related traps, both of which this suite had:

- An IMAP literal must declare its exact octet count. `BODY[] {50}` in front of a 54-byte
  body leaves the parser four bytes into the wrong place.
- If a test's client is told to SELECT, the *server* config needs an `imap_command` /
  `SELECT` rule. Without one the server has nothing to answer with, and the failure surfaces
  on the client's next expectation.

### Test Scenarios

1. **Connection & Authentication** - Verify client can connect and authenticate
2. **Mailbox Selection** - Test selecting different mailboxes (INBOX, Sent, etc.)
3. **Search Messages** - Test search criteria (UNSEEN, FROM, SUBJECT)
4. **Fetch Messages** - Test message retrieval with headers and body

## LLM Call Budget

Target: **< 10 LLM calls** for entire suite

| Test                   | LLM Calls | Notes                                             |
|------------------------|-----------|---------------------------------------------------|
| Connect & Authenticate | 2         | Server startup (1) + Client connection (1)        |
| Select Mailbox         | 2         | Server startup (1) + Client mailbox selection (1) |
| Search Messages        | 2         | Server startup (1) + Client search (1)            |
| Fetch Messages         | 2         | Server startup (1) + Client fetch (1)             |
| **Total**              | **8**     | Well under budget                                 |

## Runtime Expectations

- **Per test:** 2-4 seconds (connection, LLM call, operation, cleanup)
- **Full suite:** ~15 seconds (4 tests in parallel)
- **Timeout:** 30 seconds per test (safety margin for slow LLM responses)

## Test Infrastructure

### Server Setup

Uses NetGet IMAP server protocol (if implemented) or fallback to mock:

- Port: `{AVAILABLE_PORT}` (dynamic allocation via helpers)
- Authentication: `testuser` / `testpass`
- Mailboxes: INBOX, Sent, Drafts
- Test messages: Pre-populated via LLM instruction

### Client Configuration

IMAP clients require startup params:

```rust
NetGetConfig::new_with_startup_params(
    "Connect to 127.0.0.1:{port} via IMAP...",
    json!({
        "username": "testuser",
        "password": "testpass",
        "use_tls": false,  // TLS disabled for testing
    })
)
```

### Test Helpers

- `start_netget_server()` - Spawn server instance
- `start_netget_client()` - Spawn client instance
- `client.output_contains()` - Verify output
- `client.stop()` - Cleanup

## Known Issues

### 1. IMAP Server Dependency

**Issue:** Tests assume IMAP server is implemented in NetGet

**Workaround:** Tests will fail gracefully if IMAP server is not available. Alternative: Use external test server like
GreenMail or Docker.

**Future:** Add Docker container support for test IMAP server.

### 2. TLS Testing

**Issue:** TLS certificate validation is disabled for testing (`use_tls: false`)

**Impact:** Tests don't validate TLS handshake

**Production:** Enable TLS with proper certificate validation

### 3. Async Email Parsing

**Issue:** Email body parsing may be complex for multipart messages

**Current:** Tests use simple text emails only

**Future:** Add tests for MIME multipart messages

### 4. Search Criteria Complexity

**Issue:** LLM may struggle with complex IMAP search syntax

**Current:** Tests use simple criteria (UNSEEN, FROM, SUBJECT)

**Future:** Test advanced search combinations

## CI/CD Considerations

### Test Isolation

- **Port allocation:** Dynamic ports prevent conflicts
- **Parallel execution:** Tests can run concurrently with `--test-threads`
- **Cleanup:** Always call `.stop()` on server/client instances

### Flakiness Prevention

- **Generous timeouts:** 2-3 second sleeps for server startup
- **Output verification:** Flexible string matching (case-insensitive, substring)
- **Retry logic:** Future enhancement for network failures

### Resource Usage

- **Memory:** ~50MB per test (server + client + LLM)
- **CPU:** Moderate (LLM inference is main bottleneck)
- **Disk:** Logs written to `netget.log` (cleaned up automatically)

## Running Tests

### Single Test

```bash
./cargo-isolated.sh test --no-default-features --features imap --test client::imap::e2e_test test_imap_client_connect_and_authenticate
```

### Full Suite

```bash
./cargo-isolated.sh test --no-default-features --features imap --test client::imap::e2e_test
```

### With Logging

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features imap --test client::imap::e2e_test
```

## Future Enhancements

1. **Docker IMAP Server** - Use containerized test server (GreenMail, Dovecot)
2. **TLS Tests** - Add tests for IMAPS (port 993)
3. **OAuth2 Authentication** - Test OAuth2 flow (future feature)
4. **Message Composition** - Test draft creation and flag operations
5. **Concurrent Operations** - Test multiple simultaneous fetches
6. **Error Recovery** - Test network failures and reconnection

## Troubleshooting

### Test Hangs

- Check Ollama is running (`ollama list`)
- Verify server started (check `netget.log`)
- Increase timeout in test

### Authentication Failures

- Verify server accepts `testuser` / `testpass`
- Check startup params are correctly passed
- Review server logs for auth errors

### Connection Refused

- Server may not have started in time (increase sleep duration)
- Port may be in use (check `lsof -i :{port}`)
- Firewall blocking localhost connections

### LLM Errors

- Ollama may be down or unresponsive
- Model not available (check default model)
- Rate limiting (wait and retry)
