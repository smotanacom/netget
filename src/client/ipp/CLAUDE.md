# IPP Client Implementation

## Overview

The IPP (Internet Printing Protocol) client implementation enables LLM-controlled printing operations to remote IPP
printers. IPP is defined in RFC 8010 (encoding) and RFC 8011 (operations) and runs over HTTP, typically on port 631.

## Library Choice

**Crate**: `ipp` v5.3

- **Maturity**: Actively maintained, latest release ~2 months ago
- **Features**: Full IPP protocol support with both sync and async clients
- **Compliance**: Implements RFC 8010 and RFC 8011
- **API**: Clean, builder-pattern API with `IppOperationBuilder` and `AsyncIppClient`

## Architecture

### Connection Model

IPP is HTTP-based, so "connection" is logical rather than persistent:

1. Client stores printer URI in protocol_data
2. Each operation creates a new HTTP request
3. No persistent TCP connection maintained
4. Background task monitors for client disconnection requests

### URI Format

IPP URIs follow the pattern:

- `ipp://host:port/path` (converted to `http://`)
- `http://host:port/path` (used directly)
- Default port: 631
- Example: `http://localhost:631/printers/test-printer`

### Operations Supported

1. **Get-Printer-Attributes**: Query printer capabilities, status, supported formats
2. **Print-Job**: Submit a document for printing with job metadata
3. **Get-Job-Attributes**: Query status of a specific print job

## LLM Integration

### Event Flow

1. **Connection**: `ipp_connected` event on client initialization
2. **Operation Request**: LLM decides which operation to perform via actions
3. **Response**: `ipp_response_received` event with operation results
4. **Follow-up**: LLM can query job status or submit more jobs

### Actions

#### Async Actions (User-Triggered)

- `get_printer_attributes`: Query printer capabilities
- `print_job`: Submit a print job with job name, format, and document data
- `get_job_attributes`: Query job status by job ID
- `disconnect`: Close the client

#### Sync Actions (Response-Triggered)

- `get_printer_attributes`: Query printer after receiving a response
- `get_job_attributes`: Check job status after submitting

### Document Data Handling

The `print_job` action accepts document data as:

- **Plain text**: UTF-8 string for text/plain documents
- **Base64**: Encoded binary data for PDFs, PostScript, etc.

**The encoding is declared, never sniffed.** `document_data` is plain text unless the action
also sets `encoding: "base64"`.

It used to guess — "all ASCII alphanumeric (plus `+`, `/`, `=`) and a length divisible by four"
meant base64 — and those two sets overlap on completely ordinary documents. Printing the word
`Test` produced three bytes of binary on the wire, and nothing said so. This is the
`send_tcp_data` defect the root `CLAUDE.md` names as the reference case, in base64 rather than
hex: a string is not evidence of its own encoding, and only the sender knows.

A `base64` value that does not decode is now an error rather than being printed as its own
base64 text with the job reported as successful. Pinned by
`tests/client/ipp/document_encoding_test.rs`.

### Response Parsing

IPP responses contain attribute groups:

- **Printer Attributes**: printer-name, printer-state, media-supported, etc.
- **Job Attributes**: job-id, job-state, job-name, time-at-creation, etc.

The client extracts these into JSON structures for LLM consumption.

## Implementation Details

### Key Components

1. **IppClient::connect_with_llm_actions**: Initialize client, store URI
2. **IppClient::get_printer_attributes**: Send Get-Printer-Attributes operation
3. **IppClient::print_job**: Send Print-Job operation with document payload
4. **IppClient::get_job_attributes**: Send Get-Job-Attributes operation
5. **IppClient::call_llm_with_response**: Unified LLM callback handler

### State Management

Client state stored in `protocol_data`:

- `ipp_uri`: Full printer URI (http://...)
- `ipp_client`: Initialization marker

### Error Handling

- URI parsing errors: Return early with context
- Operation failures: Log error, notify LLM via status_tx
- Network errors: Bubble up as anyhow::Error

## Limitations

1. **No TLS Support (Yet)**: Current implementation uses HTTP only
    - Future: Add HTTPS/IPPS support via feature flag
2. **Limited Operations**: Only 3 core operations implemented
    - Future: Add Cancel-Job, Pause-Printer, Resume-Printer, etc.
3. **No Authentication**: No support for user/password or certificate auth
    - Future: Add HTTP Basic Auth and mTLS
4. **No Subscription Support**: No IPP-Subscribe or event notifications
    - Future: Implement job/printer event subscriptions
5. **Document format** is whatever `document_format` says; netget does not inspect the bytes.
    - May fail on edge cases

## Example Prompts

1. **Query Printer**:
   ```
   Connect to http://localhost:631/printers/test-printer and show me its capabilities
   ```

2. **Print Text Document**:
   ```
   Connect to ipp://localhost:631/printers/test-printer and print "Hello, World!" as a test page
   ```

3. **Print PDF** (requires base64):
   ```
   Connect to http://localhost:631/printers/pdf-printer and print this PDF: <base64-data>
   ```

4. **Check Job Status**:
   ```
   Connect to ipp://localhost:631/printers/test-printer, print a test page, then check the job status
   ```

## Testing Notes

- Requires a running IPP server (CUPS or test server)
- Default CUPS installation: `http://localhost:631/printers/<printer-name>`
- Test printer setup: Use `lpstat -p` to list available printers
- E2E tests use a local CUPS instance or mock IPP server

## Future Enhancements

1. **IPPS (IPP over TLS)**: Add rustls support for encrypted connections
2. **Extended Operations**: Cancel-Job, Hold-Job, Release-Job, Purge-Jobs
3. **Advanced Attributes**: Detailed job options (copies, sides, media, etc.)
4. **Authentication**: HTTP Basic Auth, Kerberos, client certificates
5. **IPP Everywhere**: Support for driverless printing
6. **Subscription API**: Event notifications for job/printer state changes

## Injected commands (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an IPP operation into a running client and gets back a truthful `ClientSendOutcome`. IPP is carried over HTTP, so this is the request/response archetype, not a persistent socket. See `src/client/command_support.rs` and `tests/client/ipp/command_channel_test.rs`.

- `command_support::register_command_channel` runs **before** anything that can block for a
  human — specifically before the `ipp_connected` `call_llm_for_client`, so `[ send ]` works even while an event is parked on a manual routing
  rule. This is the whole point of the feature: registering late means the rail reads
  "no command channel" for the length of the park.
- The command loop **replaced the old 5-second `get_client().is_none()` idle poll**. When the
  client is removed its handle is dropped, the channel closes, `recv()` returns `None` and the
  loop exits at once — the poll was strictly slower at the same job. The loop is registered
  with `register_client_task`, and it drops the handle on every exit path.
- One `apply_action` is the only place actions become traffic, shared by the command loop and the `ipp_connected` handler, which spawns it (after the existing 100ms printer-readiness delay) so `connect()` can return. There is
  no second wire path for injected actions to drift from.

### Outcome semantics — what `[ send ]` reports, and why

| Outcome | When |
|---|---|
| `Sent { bytes_sent }` | **Never.** the `ipp` crate owns the HTTP request and reports no wire byte count. `print_job`'s `detail` does name the document length, and says explicitly that the IPP envelope adds an unknown number of bytes on top |
| `Executed { detail }` | The operation ran to completion; `detail` names it (`Get-Printer-Attributes completed ...`) |
| `Rejected { error }` | `execute_action` refused the JSON (unknown verb, missing field) |
| `Disconnected` | `disconnect`; the loop ends, the handle is dropped and the client goes to `Disconnected` |
| `Err(...)` | The request itself failed (transport error, non-2xx). The caller sees the error rather than a false success |

**The known cost of awaiting.** `apply_action` awaits the whole operation, *including the
response event it raises*. That is what makes the outcome truthful, but it also means the
command loop is busy until that event has been handled — with a `*` -> manual rule, until the
human answers it or the intercept times out (default 300s). Injected sends queue behind it on
the bounded channel, which surfaces as "client busy" backpressure. `send_to_client`'s own
timeout protects the caller either way.

**Not wired:** `call_llm_with_response` still discards the actions the LLM returns for `ipp_response_received` (`actions: _`). That predates the command channel and is untouched by it; the injected path is currently the only way to run a follow-up `get_job_attributes` without restarting the client.
