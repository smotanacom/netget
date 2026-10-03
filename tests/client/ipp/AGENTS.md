# IPP Client E2E Test Strategy

## Overview

End-to-end tests for the IPP client verify LLM-controlled printing operations against a real or mock IPP print server.

## Test Philosophy

**Black-box testing**: Tests validate that the LLM can successfully:

1. Connect to an IPP printer
2. Query printer capabilities
3. Submit print jobs
4. Check job status

Tests use **real IPP operations** (not mocked responses) to ensure correct protocol behavior.

## Test Suite

### Test 1: `test_ipp_get_printer_attributes`

**Purpose**: Verify the client can query printer capabilities

**Setup**:

- IPP client connects to `http://localhost:631/printers/test-printer`
- LLM instruction: "Query the printer and tell me its capabilities"

**Actions**:

1. Client connects
2. Manually trigger `get_printer_attributes` operation
3. Verify response received
4. Check status messages for success

**Expected LLM Calls**: 1-2

- Initial connection (optional)
- Response processing

**Runtime**: ~5-8 seconds

**Validation**:

- Client status is `Connected`
- Status messages contain "IPP client" and "received response"

---

### Test 2: `test_ipp_print_job`

**Purpose**: Verify the client can submit a print job

**Setup**:

- IPP client connects to test printer
- LLM instruction: "Print a test page with the text 'NetGet IPP Test'"

**Actions**:

1. Client connects
2. Manually trigger `print_job` with test document
3. Verify print job submitted successfully
4. Check for job response

**Expected LLM Calls**: 1-2

- Initial connection (optional)
- Print job response processing

**Runtime**: ~5-8 seconds

**Validation**:

- Client status is `Connected`
- Status messages contain "Print-Job" or "print_job"
- No error messages

---

### Test 3: `test_ipp_full_workflow`

**Purpose**: Comprehensive workflow test (query → print → check status)

**Setup**:

- IPP client connects to test printer
- LLM instruction: "First query the printer capabilities, then print a test page, and finally check the job status"

**Actions**:

1. Client connects
2. Query printer attributes
3. Submit print job
4. Query job status (with placeholder job_id)

**Expected LLM Calls**: 3-4

- Connection
- Each operation response

**Runtime**: ~10-15 seconds

**Validation**:

- All operations complete without errors
- Client remains connected throughout

---

## LLM Call Budget

**Total for suite**: < 10 LLM calls

Breakdown:

- Test 1: 1-2 calls
- Test 2: 1-2 calls
- Test 3: 3-4 calls
- **Total**: 5-8 calls (well under budget)

## Prerequisites

### Required Services

1. **IPP/CUPS Server**:
    - Install CUPS: `sudo apt-get install cups` (Debian/Ubuntu) or `brew install cups` (macOS)
    - Start CUPS: `sudo systemctl start cups`
    - Add test printer:
      ```bash
      lpadmin -p test-printer -E -v file:/dev/null -m drv:///sample.drv/generic.ppd
      ```
    - Verify: `lpstat -p test-printer`

2. **Ollama**:
    - Running on default endpoint
    - Model available (e.g., qwen3-coder:30b)

### Alternative: Mock Server

For CI/CD environments without CUPS, consider:

- Mock IPP server that responds to Get-Printer-Attributes and Print-Job
- Minimal HTTP server returning valid IPP response bytes
- Or use Docker: `docker run -d -p 631:631 cups/cups`

## Running Tests

```bash
# All IPP client tests (requires CUPS and Ollama)
./cargo-isolated.sh test --no-default-features --features ipp --test client::ipp::e2e_test -- --ignored

# Specific test
./cargo-isolated.sh test --no-default-features --features ipp test_ipp_get_printer_attributes -- --ignored

# Without Ollama lock (faster, but may conflict)
./cargo-isolated.sh test --no-default-features --features ipp --test client::ipp::e2e_test
```

**None of these tests is `#[ignore]`d and none needs CUPS.** This section described a state the
suite left behind: `e2e_test.rs` runs against a mocked model and a NetGet IPP server of our own,
`command_channel_test.rs` costs zero LLM calls, and `document_encoding_test.rs` drives the
executor directly with no socket at all. The "Prerequisites" above are useful for *manual*
verification against a real CUPS and nothing more — a `-- --ignored` run finds nothing here.

The suite is also three files, not the one this page used to describe, and there is no
`test_ipp_full_workflow`; the third e2e case is `test_ipp_get_job_attributes`. Derive the list
rather than trusting it:

```bash
./cargo-isolated.sh test --no-default-features --features ipp --test client -- --test-threads=100 client::ipp
```

### `document_encoding_test.rs` — the encoding is declared, never sniffed

Six tests, no LLM calls, no sockets. `print_job` used to guess whether `document_data` was
base64 from its character set and length, and the two sets overlap: `Test`, `Note`, `Data` and
every other four-character alphanumeric string decoded into three bytes of binary. The test
that matters is `a_four_character_document_is_printed_as_itself`; the rest pin that declared
base64 still decodes, that invalid base64 is refused rather than printed as its own text, that
an unknown encoding is refused, and that a real PDF survives the round trip.

## Known Issues

1. **Job ID Extraction**: Current implementation doesn't parse job_id from Print-Job response
    - Test 3 uses placeholder job_id
    - Fix: Parse job-id attribute from response

2. **CUPS Permissions**: May require user to be in `lpadmin` group
    - Fix: `sudo usermod -a -G lpadmin $USER` and re-login

3. **Printer Not Found**: If test printer doesn't exist, operations fail
    - Fix: Create test printer as shown in Prerequisites

4. **Network Timeouts**: IPP operations may time out on slow systems
    - Fix: Increase sleep durations in tests

## Future Improvements

1. **Mock IPP Server**: Bundle a minimal IPP server for CI/CD
2. **Job ID Parsing**: Extract and use real job IDs from Print-Job responses
3. **Attribute Validation**: Assert specific printer attributes in responses
4. **Error Cases**: Test invalid URIs, missing printers, malformed requests
5. **LLM-Driven Tests**: Let LLM generate actions instead of manual triggers
6. **TLS Support**: Test IPPS (IPP over TLS) when implemented

## Maintenance Notes

- **CUPS Version**: Tests assume CUPS 2.x compatibility
- **Port Conflicts**: Default port 631 may conflict with system CUPS
    - Use test instance on alternate port if needed
- **Cleanup**: Tests don't delete print jobs, may accumulate in queue
    - Periodic cleanup: `cancel -a -x` (cancel all jobs)

## Test Efficiency Checklist

✅ **LLM Calls**: < 10 (5-8 actual)
✅ **Runtime**: < 30 seconds total
✅ **No External Network**: Localhost only
✅ **Feature Gated**: `#[cfg(all(test, feature = "ipp"))]`
✅ **Ignored by Default**: Requires `-- --ignored` flag
✅ **Documented Prerequisites**: CUPS and Ollama
✅ **Black-box Approach**: Uses public APIs only

## `command_channel_test.rs` — the dashboard's `[ send ]`

One test, **zero LLM calls**. The client's LLM points at `http://127.0.0.1:1`, so the
connected-event call (where there is one) and every response-event call fail immediately;
tolerating that failure is part of what the test verifies.

The printer is a **NetGet IPP server of our own**, started with a `*` static handler that answers `ipp_printer_attributes`. IPP rides on HTTP, so the whole exchange is localhost TCP; nothing contacts an external endpoint.

What it asserts, in order:

1. `wait_for_client_handle` — the regression guard for "register the channel before the
   connected-event LLM call". Without it, a manual routing rule would leave `[ send ]` dead
   for the length of the park.
2. The injected action returns `Executed`, **not** `Sent` — the `ipp` crate owns the HTTP request and reports no wire byte count.
3. The server's access log contains the printer name it answered with, proving the injected operation crossed the wire and came back.
4. The client's access log gained an `injected_action` entry, so injected traffic shows up in
   the request pane exactly like LLM-produced traffic.
5. An unknown verb comes back `Rejected`, not silently swallowed.
6. `disconnect` returns `Disconnected`, the command loop exits, the handle is dropped and the
   client's status becomes `Disconnected`.
