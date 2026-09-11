# CoAP Protocol Implementation

## Overview

CoAP (RFC 7252) server over UDP. The LLM plays a constrained device: it decides what a resource
*is* — its representation and media type — and which response code the request deserves. NetGet
owns the message layer: types, message ids, tokens, options and the replies the specification
leaves no room to decide.

**Status**: Beta — `metadata()` says so, and the evidence is
`test_coap_get_post_and_not_found_with_coap_client` plus
`test_coap_message_layer_echo_and_ping`, driven by the `coap` 0.27 client over `coap-lite`
0.13. Neither is `#[ignore]`d and neither has a skip-when-missing gate, and both crates are
**unconditional** dev-dependencies, so the evidence compiles wherever the suite does. (This
line said "Experimental" while the code said `Beta` — the code was right.) The blocking CI
`test` job runs `tcp,http,dns,udp,redis,mcp-stdio` only and so does not execute it; run it
yourself.
**RFC**: RFC 7252 (Constrained Application Protocol)
**Port**: 5683, declared as `PrivilegeRequirement::None`
**Feature**: `coap` (no optional dependencies — the codec is hand-rolled)

5683 is above 1023, so a `PrivilegedPort` declaration here could never fire and would read as
protection that is really dead code. `None` is the honest declaration.

## Library choice

**Hand-rolled**, in `codec.rs` (~470 lines). `coap-lite` is a good codec and would have worked,
but the test peer is `coap-lite` (and the `coap` crate on top of it). Using it on both sides
would make the round-trip tests a tautology: a shared encoder cannot disagree with itself. With
a hand-rolled server, "coap-lite decoded our ACK and found the right token" is real evidence.

Both test crates are dev-dependencies only: `coap-lite` 0.13 (MIT OR Apache-2.0) and `coap` 0.27
(MIT).

## What is implemented

- The 4-byte header: version, type, token length, code, message id.
- All four message types: CON, NON, ACK, RST.
- Tokens, 0-8 bytes, echoed verbatim on every response.
- Option encoding and decoding including **both** extension forms — nibble 13 (one extra byte,
  values 13-268) and nibble 14 (two extra bytes, 269+) — for delta and for length.
- Uri-Path, Uri-Query, Content-Format and Accept surfaced to the model; every other option is
  decoded and preserved in the message but not interpreted.
- The payload marker `0xFF`.
- The request codes GET/POST/PUT/DELETE and the full `class.detail` response code space.
- Piggybacked responses: CON → ACK with the request's message id; NON → NON with a fresh one.
- CoAP ping (RFC 7252 §4.3): an empty CON is answered with RST.
- RST in reply to a malformed CON (§4.2).

**Not implemented**: Observe (RFC 7641), Block-wise transfer (RFC 7959), DTLS / CoAPS on 5684,
separate (delayed, non-piggybacked) responses, retransmission of Confirmable responses, and
multicast. `.well-known/core` is not special-cased: the model can serve it, but nothing
generates a link-format listing automatically, because there is no resource registry to list.

## Architecture decisions

### 1. No resource database — the model answers every request

No filesystem, no map of paths to representations, no persistence. `coap_request` fires and the
model decides. Continuity across requests comes from server memory (`set_memory`,
`append_memory`) or the generic SQLite facility, never from protocol-local storage.

### 2. Message ids and tokens are server-side, deliberately

`codec::response_to` takes the type, message id and token from the request. The model does not
see the token at all and sees the message id only as an informational event field it is told to
ignore.

This is the opposite of what DNS does (where `query_id` is an action parameter the model must
echo), and it is a deliberate improvement:

- Reliability matching is not a decision. Getting it wrong means the peer retransmits forever;
  there is no interesting way to be creative about it.
- It removes the failure mode where a static handler is useless because it cannot echo a random
  identifier. Static and script handlers work fine for CoAP.

The consequence for tests is spelled out in `tests/server/coap/CLAUDE.md`: the mocks still use
`respond_with_actions_from_event()`, but what they derive dynamically is the **path**, and the
message-id/token echo is asserted directly against `coap-lite`-decoded replies instead.

### 3. Mechanical replies never reach the model

Answered in `mod.rs` with no LLM call:

- Empty CON (a ping) → RST.
- Malformed datagram that was Confirmable → RST for its message id.
- Empty NON/ACK/RST → ignored.
- A message carrying a *response* code, arriving at a server → ignored.
- A class-0 code that is not 1-4 → 4.05 Method Not Allowed (RFC 7252 §5.8 defines only four).

### 4. Payload encoding is explicit and actually decoded

`send_coap_response` takes `payload` plus an optional `encoding` of `"utf8"` (default) or
`"hex"`, and `decode_payload()` really decodes the hex — tolerating whitespace, `:` separators
and a leading `0x`, and erroring with a legible message otherwise. There is no sniffing:
`"48656c6c6f"` is simultaneously valid text and valid hex and only the sender knows which it
means. This is the `send_tcp_data` lesson applied up front rather than after the fact.

`content_format` accepts a media type name (`"application/json"`, `"application/cbor"`, ...) or
the numeric identifier. When a payload is present and `content_format` is omitted it defaults to
`text/plain;charset=utf-8` (identifier 0), so a client is never handed bytes with no media type;
when there is no payload, no Content-Format option is emitted.

### 5. Fail closed

`outcome_from_results` answers **5.03 Service Unavailable** with an ERROR log when the LLM call
failed or when no usable action came back. It never invents an empty 2.05, which the client
would believe.

Note the three outcomes are structurally distinct: a response, a Reset, and deliberate silence
(`ignore_coap_request`). "The model said nothing" and "the model chose to say nothing" are
different code paths.

### 5b. `decision=` separates a backend outage from the model choosing 5.03

5.03 is what both fail-closed paths put on the wire *and* a code the model may legitimately
pick itself, so the peer cannot tell them apart. The log carries a stable token per request,
as `src/server/radius/` does:

| token | meaning | level |
|---|---|---|
| `model_answer` | the model served the resource (class 2) | DEBUG |
| `model_reject` | the model refused, class 4 or 5 | DEBUG |
| `model_reset` | the model chose RST | DEBUG |
| `model_silent` | the model chose `ignore_coap_request` — a decision, not an absence | DEBUG |
| `spec_reply` | ping → RST, or unrecognised method → 4.05; the model was never asked | DEBUG/WARN |
| `fail_closed_llm_error` | the LLM backend failed | **ERROR** |
| `fail_closed_no_action` | the model was asked and returned nothing usable | **ERROR** |

`grep 'decision=fail_closed_'` finds every request the model did not actually answer. The
error text stays in the log and never reaches the wire — only the code does.

### 6. A response has to fit one datagram, and the bound is on the model's side

`codec::MAX_PAYLOAD_LEN` is **1024 bytes** and `decode_payload` enforces it, counting decoded
bytes so a `hex` payload gets no extra budget. RFC 7252 §4.6 is where the number comes from:
with the path MTU unknown an endpoint assumes `MAX_MESSAGE_SIZE` of 1152, "leading to a
maximum payload size of 1024 bytes", and Block-wise transfer (RFC 7959) — the legal way to
exceed it — is not implemented here.

The limitation was documented and unenforced, which is the worse combination. Nothing stopped
the model returning a 100 KB representation; the oversize datagram then reached `send_to`,
failed with `EMSGSIZE`, was logged, and **nothing was sent** — so a CON client retransmitted
four times and gave up with no diagnostic on its side. Refusing at `execute_action` instead
turns it into a legible error the model can act on (return less), and the existing fail-closed
path answers 5.03 if it does not.
`tests/server/coap/e2e_test.rs::test_oversize_payload_is_refused_rather_than_silently_dropped`
pins the limit, the message, and that exactly 1024 bytes is still accepted.

## LLM integration

### Events

One event, `coap_request`, emitted for every well-formed GET/POST/PUT/DELETE. Its data:

| Field | Notes |
|---|---|
| `method` | GET / POST / PUT / DELETE |
| `path` | assembled from Uri-Path options, always leading-slashed; `/` when absent |
| `path_segments` | the same, unjoined |
| `query` | Uri-Query options joined with `&`; absent when there was none |
| `message_type` | `CON` or `NON` — informational; the reply is matched automatically |
| `message_id` | informational; echoed automatically |
| `content_format` | media type of the request body, when declared |
| `accept` | media type the client asked for, when it sent an Accept option |
| `payload` / `payload_encoding` | request body, `utf8` when all bytes are printable, otherwise `hex` |

### Actions

- `send_coap_response { code, payload?, encoding?, content_format? }` — `code` is written
  `class.detail` (`"2.05"`, `"4.04"`, `"5.03"`); class must be 2, 4 or 5. The decoded payload
  must be at most 1024 bytes.
- `send_coap_reset {}` — RST. For a message the device cannot make sense of at all, not for
  "not found".
- `ignore_coap_request {}` — send nothing, modelling a sleeping or unreachable node.

## Startup parameters

**None.** `get_startup_parameters()` is not overridden, so there is nothing declared and nothing
read — no dead parameter for the model to reach for.

## Why `.connectionless()` is right here

The test is not "is it UDP" but "has it a connection concept" — declaring `.connectionless()`
on a UDP protocol that carries a *transfer* is actively harmful, because the 10-second idle
sweep then evicts a live transfer that is merely waiting on an LLM call (TFTP had exactly
that). CoAP as implemented here has no session: Observe and Block-wise are the two features
that would give it one and neither exists, so a datagram is answered and forgotten, and the
per-datagram connection entry is genuinely disposable. The one visible consequence is that a
request parked longer than 10 seconds can have its sent-byte counter land on an entry the
sweep already removed; nothing about the reply itself depends on it.

## Known limitations

1. **No Observe, no Block-wise.** A response therefore has to fit one datagram: the receive
   buffer is 2048 bytes and a response payload is capped at `codec::MAX_PAYLOAD_LEN` (1024,
   per RFC 7252 §4.6) — see decision 6. A request larger than the 2048-byte receive buffer is
   truncated by the kernel and then fails to decode, which produces an RST for a CON.
2. **No DTLS**, so no CoAPS on 5684.
3. **Only piggybacked responses.** A slow model still answers in the ACK rather than sending an
   empty ACK followed by a separate CON, so a client with a short ACK_TIMEOUT (2s by default)
   may retransmit while an LLM call is in flight. Use script or static handlers when that
   matters.
4. **No deduplication cache.** A retransmitted CON produces a second event and a second LLM
   call. RFC 7252 §4.5 permits re-processing an idempotent request; it is still a cost.
5. **`get_async_actions()` is empty**, because a plain CoAP server has nothing to say
   unprompted. Unsolicited notification is exactly what Observe is for, and it is not
   implemented.

## Example prompts

```
listen on port 5683 via coap
You are a soil moisture sensor.
GET /sensors/moisture returns 2.05 with JSON {"pct": <0-100>, "ts": <unix seconds>}.
GET /sensors/temp returns 2.05 with plain text degrees Celsius.
PUT /config/interval accepts an integer number of seconds and returns 2.04.
Everything else is 4.04.
```

```
listen on port 5683 via coap
Impersonate a smart lock. GET /lock/state returns "locked" or "unlocked" as
text/plain. POST /lock/actuate with body "unlock" returns 2.04 only if memory
says a valid PIN was presented in the last 30 seconds; otherwise 4.01.
```

## References

- [RFC 7252: The Constrained Application Protocol (CoAP)](https://datatracker.ietf.org/doc/html/rfc7252)
- [RFC 7641: Observing Resources in CoAP](https://datatracker.ietf.org/doc/html/rfc7641) — not implemented
- [RFC 7959: Block-Wise Transfers in CoAP](https://datatracker.ietf.org/doc/html/rfc7959) — not implemented
- [coap-lite](https://docs.rs/coap-lite) / [coap](https://docs.rs/coap) — the test peers, not runtime dependencies
