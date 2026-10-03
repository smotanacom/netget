# IPP Protocol E2E Tests

## Test Overview

Three tests in `test.rs` covering IPP (RFC 8010 encoding, RFC 8011 semantics) over HTTP. Each starts
its own server, POSTs a hand-built IPP request, and decodes the reply.

## Test Strategy

**Decode the response, don't check the status code.** The suite used to assert only
`status() == 200` and print the first bytes. That is why three bugs a real client rejects could sit
in the encoder while the tests stayed green: `hex::encode` of ASCII text on the wire, every attribute
emitted as `nameWithoutLanguage` regardless of type, and a hardcoded version 2.0 and request-id 1.

`decode_ipp` is written by hand and strictly — a lenient parser here would reproduce the original
problem. It asserts the message ends at its `0x03` terminator with no trailing bytes, follows
delimiter tags into attribute groups, and handles the zero-length-name continuation of RFC 8010
additional values.

**Never send the defaults.** `ipp_request()` takes the version and request-id from the caller, and
each test passes values that are neither 2.0 nor 1 — `(1,1)`/`0x12345678`, `(2,0)`/`0x2A7`,
`(1,1)`/`0xBEEF01`. A request using the old defaults could not have detected either hardcoding bug.

`assert_response_envelope` then requires, for every operation: the version echoed, the request-id
echoed, `successful-ok` (0x0000), and `attributes-charset` then `attributes-natural-language` as the
first two attributes of the operation group with the right tags and values (RFC 8010 §3.1.4).

## LLM Call Budget

**Total: 6** — 3 servers × (1 startup + 1 `ipp_request_received`). Every rule is `expect_calls(1)`.

## Test Cases

### 1. `test_ipp_get_printer_attributes` (operation 0x000B)

Handler returns `ipp_printer_attributes`. Asserts HTTP 200 with `Content-Type: application/ipp`, the
envelope, and that all three attributes land in the **printer** group with the value tag their JSON
type implies:

- `printer-name` → `nameWithoutLanguage` (0x42), `"NetGet Printer"`
- `printer-state` → `enum` (0x23) with value **3** — `"idle"` must become the enum, not text
- `printer-uri-supported` → `uri` (0x45)

The event rule also requires `answer_with` to name `ipp_printer_attributes` — the server's
per-operation hint, without which the rule does not match and the test fails. Verified by deleting
the field from the event: this test and `test_ipp_print_job` both fail.

### 2. `test_ipp_print_job` (operation 0x0002)

Document data follows the end-of-attributes tag. Handler returns `ipp_job_attributes`. Asserts the
attributes land in the **job** group:

- `job-id` → `integer` (0x21), 1 — a number must not go out as text
- `job-state` → `enum` (0x23) with value **5** for `"processing"`
- `job-name` → `nameWithoutLanguage` (0x42), `"test"`

The event rule requires `answer_with` to name `ipp_job_attributes` for a Print-Job.

### 3. `test_ipp_status_only_response`

Handler returns `ipp_response` with `ipp_status: "server-error-not-accepting-jobs"`. Asserts the reply
still rides on **HTTP 200** (RFC 8011: an IPP error is an IPP status, not an HTTP one), that version
and request-id are echoed even on an error, that the status is `0x0506`, that the handler's
`status-message` reaches the client, and that no printer or job group was invented.

This mock previously returned `{"type": "http_response", …}`, which IPP has no executor for — the
server rejected it and answered with a fallback, and the test passed anyway because it only asserted
"2xx or 405". Both halves are fixed.

## Client Library

`reqwest` for HTTP. IPP encoding and decoding are hand-rolled in `test.rs`; there is no suitable
Rust IPP client library.

## Expected Runtime

~0.5s for the whole suite against the mock harness.

## Not Covered

Get-Jobs, Cancel-Job, Get-Job-Attributes; multi-value attributes; `1setOf` collections; IPP over TLS;
CUPS `ipptool` compliance runs; script mode.
