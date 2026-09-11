# IPP Protocol Implementation

**Status**: `DevelopmentState::Experimental`.
**Privilege**: `PrivilegeRequirement::PrivilegedPort(631)` — 631 is below 1024 and is where every
IPP client looks by default, so the preflight check in `server_startup.rs` fires rather than
letting the bind fail with a bare EPERM.

IPP/1.1 and 2.0 (RFC 8010 wire format, RFC 8011 semantics) over HTTP POST. `hyper` carries the
HTTP; the IPP body is parsed and built here.

## No storage

There is no job queue and no printer state. A `Print-Job` is not recorded anywhere; a
following `Get-Job-Attributes` is answered by the model out of its own memory. Printer
attributes likewise come from the model on every request.

`list_print_jobs` used to be declared as an async action and returned a hardcoded
`{"jobs": []}` next to a `// This is a placeholder` comment. It was removed rather than
implemented: implementing it would mean keeping a job store, which is exactly what a protocol
must not do.

## Actions

Three, all advertised on `ipp_request_received`:

| action | purpose | key fields |
|---|---|---|
| `ipp_response` | status-only answer (acknowledge, reject) | `ipp_status`, `status_message`, `http_status` |
| `ipp_printer_attributes` | Get-Printer-Attributes | `attributes` (object), `ipp_status` |
| `ipp_job_attributes` | Print-Job / Get-Job-Attributes / Create-Job | `attributes` (object), `ipp_status` |

`ipp_status` is a **name**, not a number: `successful-ok`, `client-error-not-found`,
`server-error-not-accepting-jobs`, and so on (full list in `ipp_status_code`). An unrecognised
name encodes `server-error-internal-error` and logs a warning — telling a client everything is
fine because the model invented a status name is the worse failure.

`http_status` is separate and rarely needed: IPP errors belong in `ipp_status` with HTTP 200.
`status` is accepted as an alias for `http_status` because prompts in the wild use it.

### The bytes-in-an-action bug that was removed

`ipp_response` used to take a `body` parameter documented in three places as
"hex-encoded IPP response data", with examples like `"body": "02000000000000010347001200..."`.
The executor did:

```rust
let body = action.get("body").and_then(|v| v.as_str()).unwrap_or("");
... "body": hex::encode(body.as_bytes())
```

`hex::encode` of the *ASCII text*. A model following the documentation put the literal
characters `0`, `2`, `0`, `0`, … on the wire as the IPP response body. Every such response was
unparseable garbage.

The parameter is gone. Nothing the model writes reaches the wire as bytes; it names a status
and supplies attributes, and the encoder here produces the message. (Hex still appears in
`ActionResult::Custom`'s `body_hex` between the executor and the HTTP handler, because
`Custom` carries a `serde_json::Value` and JSON has no byte type. That is server-internal —
the model never sees or writes it.)

## Attribute encoding

`push_attribute_value` picks the IPP value tag from the JSON type and the attribute name:

| JSON | attribute name | tag |
|---|---|---|
| number | any | `integer` (0x21) |
| bool | any | `boolean` (0x22) |
| string | `printer-state`, `job-state` | `enum` (0x23), mapped from `idle`/`processing`/`completed`/… |
| string | contains `charset` | `charset` (0x47) |
| string | contains `natural-language` | `naturalLanguage` (0x48) |
| string | ends `-uri` / `-uri-supported` | `uri` (0x45) |
| string | contains `document-format` | `mimeMediaType` (0x49) |
| string | ends `-name` | `nameWithoutLanguage` (0x42) |
| string | ends `-supported`/`-default`/`-requested`/`-reasons` | `keyword` (0x44) |
| string | otherwise | `textWithoutLanguage` (0x41) |
| array | any | first value carries the name, the rest a zero-length name (RFC 8010 additional value) |

Everything used to be encoded as `nameWithoutLanguage` regardless of type, so integers went
out as decimal text and `printer-state` as a keyword where clients expect an enum. Name
lengths were also written as `[0x00, len as u8]`, silently truncating any name of 256 bytes or
more into an unparseable message; they are now proper two-byte big-endian.

Known gap: `operations-supported` is `1setOf type2 enum` in the spec and is encoded here as
`1setOf integer` because the values arrive as JSON numbers. `ipptool` accepts it; a strict
client might not.

### Numbers are range-checked before they are narrowed

`validate_attributes` runs in the executor, **before** `build_ipp_response`, and refuses any
integer outside i32 and any name or value past 65535 bytes. Both are cases where the encoder
used to rewrite the model's answer in silence:

- `n.as_i64().unwrap_or(0) as i32` made `job-id: 4294967296` into **0**, and a client reading
  job 0 has no way to know it was ever anything else. Same for `1.5`, which `unwrap_or(0)` also
  turned into 0 rather than reporting that IPP's `integer` syntax has no fractional form.
- `push_len_prefixed` clamped to `u16::MAX` and then wrote a message *declaring* the truncated
  length — self-consistent, parseable, and not what the model said.

The check has to come first: `as i32` on an out-of-range value leaves nothing to check
afterwards. That is the property `tests/narrowing_cast_drift_test.rs` encodes generally, and
`tests/server/ipp/attribute_range_test.rs` encodes for this encoder, arrays included — a
multi-valued attribute is the easy place for a bound to be forgotten, because the value the
encoder narrows is one level below the one a naive check looks at.

Refusing beats clamping: `i32::MAX` is not what the model asked for either, and the message is
what the repair loop reads.

## Request-id and version are echoed by the server, not by the model

RFC 8011 requires a response to carry the request's request-id and the request's version. Both
are parsed from the 8-byte header in `parse_ipp_header` and written into the encoded response
by `stamp_response_header` — the encoders emit version 2.0 and request-id 0 as placeholders.

No action carries either field, deliberately: correlation must not depend on a model repeating
a number back. That is the same failure mode that made DNS and NTP responses go unmatched.

Both were real bugs, both caught by a real client:

- request-id was **hardcoded to 1** in the attribute builders. A client using any other id got
  a response it should discard as unmatched.
- version was **hardcoded to 2.0**. `ipptool` speaks 1.1 by default and failed every response
  with `Bad version 2.0 in response - expected 1.1 (RFC 2911 section 3.1.8)` — this was the
  only failure in an otherwise perfect decode, and it would have failed against CUPS too.

## Events

One: `ipp_request_received`, carrying `method`, `uri`, `operation`, `request_id`,
`ipp_version`. It advertises all three response actions via `.with_actions(...)`.

The response example used to be `{"type": "placeholder", "event_id": "ipp_request_received"}`,
which is rendered verbatim into the prompt and taught the model an action named `placeholder`.
It is now a real `ipp_printer_attributes` response, with `ipp_job_attributes` and a rejecting
`ipp_response` as alternatives.

## Request parsing is shallow

Only the 8-byte header is decoded. **Attribute groups in the request are not parsed**, so the
model is told the operation name but not which `printer-uri` was asked about, which
`document-format` was requested, or what a `Print-Job`'s document data contains. If the model
needs any of that, decode the request's attribute groups in `handle_ipp_request_with_llm` and
add them to the event.

`parse_ipp_header` reads five fixed offsets after one `body.len() < 8` check; there is no
attacker-controlled length arithmetic in it.

## The body is bounded before it is buffered

`Incoming` has no default limit, so `req.into_body().collect()` let an unauthenticated `POST`
to port 631 decide how much memory this server allocated. It is read through
`http_body_util::Limited` at `MAX_IPP_BODY_BYTES` (8 MiB, the same figure `http_common` uses;
spelled out locally because that module is gated behind the `http`/`http2` features and `ipp`
implies neither).

A refusal is expressed twice: HTTP **413** for a generic client, and
`client-error-request-entity-too-large` (0x0408) in the body for one that reads only the IPP
layer. It costs **no LLM call**, and that is the assertion `tests/server/ipp/body_limit_test.rs`
actually makes (`expect_calls(0)`), because the old code turned a read failure into
`Bytes::new()` — which `parse_ipp_header` reports as the operation `Empty`, so the model would
have been asked to answer a request nobody sent.

Nothing recursive is decoded here, which is why there is no depth bound: only the 8-byte header
is parsed. If request attribute groups are ever decoded, the walk must stay iterative — a Rust
stack overflow is a `SIGSEGV`, not a panic, so `tokio::spawn` cannot contain it and the whole
process dies.

## Failure behaviour

Every outcome answers with a parseable IPP message, and the `decision=` tag is what tells them
apart — on the wire three of them are byte-identical, and one of those is a status the model
can also choose deliberately.

| Outcome | Wire | `decision=` tag |
|---|---|---|
| The model answered | its `ipp_status`, its `http_status` | `model_answer` |
| No `ipp_*` action came back | 200 + `server-error-internal-error` | `model_silent` |
| An action came back and the executor refused it | 200 + `server-error-internal-error` | `fail_closed_action_error` |
| The backend failed, saturated | 200 + `server-error-busy` | `fail_closed_llm_error` |
| The backend failed otherwise | 200 + `server-error-internal-error` | `fail_closed_llm_error` |
| Body over the cap | 413 + `client-error-request-entity-too-large` | `fail_closed_body_too_large` |
| The encoder's own hex would not decode | 200 + `server-error-internal-error` | `fail_closed_encoding_bug` |

`WireFailure::classify` picks between the saturated and the general case; the error **text**
never reaches the wire, only the log. `server-error-busy` rather than
`server-error-internal-error` for a saturated backend is the same distinction `http` draws with
503 vs 500 — it is what a client retries on.

Two paths used to give a client something it could not parse, and both are gone:

- **LLM error** → HTTP 500 with the text body `Internal Server Error`. An IPP client reports
  that as a protocol error with no useful detail.
- **LLM returned no `ipp_*` action** → HTTP 200 with an *empty* body, which is not a valid IPP
  message; clients report a truncated response.

A third was still live until this pass: `hex::decode(body_hex).unwrap_or_default()` turned a
decode failure into that same empty body. The hex is written and read entirely inside this
protocol, so a failure means the two halves have drifted; it is now loud and answered with
something parseable.

## Connection tracking

`update_connection_stats` is called for the request body and every response, so an IPP server's
peers show live byte counters in the dashboard rail. Nothing did this before; the counters read
zero for the life of the server however much it printed.

`call_llm` is passed `Some(connection_id)`. It was `None` behind a `// TODO: Add connection_id
when available` while the id had been in scope the whole time, which made connection-scoped
event handlers and connection-scoped scheduled tasks silently inapplicable to every IPP request.

## Limitations

- Every request is one LLM call and nothing bounds the rate, so an unauthenticated request loop
  is an unauthenticated model-call loop. This is protocol-wide in netget rather than specific to
  IPP; `src/server/tuntap/` is the one protocol here that solves it (a filter plus a rolling
  per-minute window).
- No IPPS (IPP over TLS), no authentication.
- No CUPS extensions.
- Request attribute groups not parsed (above).
- No document data: `Print-Job` bodies are read into memory to size them and then discarded.
- `operations-supported` encoded as integer rather than enum (above).
- No job store, by design (above).

## Manual verification

```bash
./cargo-isolated.sh run --release --no-default-features --features ipp
# start on 10631 with a static ipp_printer_attributes handler, then:

cat > /tmp/getattrs.test <<'EOF'
{
    NAME "Get-Printer-Attributes"
    OPERATION Get-Printer-Attributes
    GROUP operation-attributes-tag
    ATTR charset attributes-charset utf-8
    ATTR language attributes-natural-language en
    ATTR uri printer-uri $uri
    STATUS successful-ok
    DISPLAY printer-name
    DISPLAY printer-state
}
EOF
ipptool -tv ipp://127.0.0.1:10631/printers/netget /tmp/getattrs.test
```

Verified output (2026-08-05): `[PASS]`, with `printer-name (nameWithoutLanguage)`,
`printer-state (enum) = idle`, `printer-is-accepting-jobs (boolean) = true`,
`printer-uri-supported (uri)`, `document-format-supported (1setOf mimeMediaType)`,
`operations-supported (1setOf integer)` — i.e. every value tag decoded as intended. A raw POST
with request-id `0x12345678` and version 1.1 came back with exactly that id and version.

## Testing

Four files, 12 tests, none `#[ignore]`d and none skip-when-missing:

- `tests/server/ipp/test.rs` — Get-Printer-Attributes, Print-Job and a status-only reply, each
  decoded with a hand-written strict decoder. An earlier version asserted only HTTP 200 and
  printed the first bytes, which is why the value-tag and version bugs survived it.
- `status_range_test.rs` — `http_status` refused rather than wrapped.
- `attribute_range_test.rs` — attribute integers and lengths refused rather than narrowed or
  truncated, arrays included.
- `body_limit_test.rs` — an oversized body refused with 413 and **zero** LLM calls, and a 1 MiB
  one still served.

`ipptool` remains the check for spec compliance that no in-tree test covers; see Manual
verification above.

## References

- [RFC 8010: IPP/1.1 Encoding and Transport](https://tools.ietf.org/html/rfc8010)
- [RFC 8011: IPP/1.1 Model and Semantics](https://tools.ietf.org/html/rfc8011)
- [PWG IPP registrations](https://www.pwg.org/ipp/ipp-registrations.xml)
