# CoAP Protocol Implementation

## Overview

CoAP (RFC 7252) server over UDP. The LLM plays a constrained device: it decides what a resource
*is* — its representation and media type — and which response code the request deserves. NetGet
owns the message layer: types, message ids, tokens, options and the replies the specification
leaves no room to decide.

**Status**: **Stable**, set 16 September 2026. See "Maturity: the six conditions" at the foot
of this file for what was checked, what failed when it was checked, and what the rating does
*not* cover.

This line used to name only `test_coap_get_post_and_not_found_with_coap_client` and
`test_coap_message_layer_echo_and_ping`, the two Rust-crate tests, and say nothing about
`real_client_test.rs`. That omitted the strongest evidence the protocol has: libcoap's own
`coap-client`, a C implementation, has driven this server since September 2026. Three peers,
none of which shares a line with `codec.rs`:

| peer | what it is | test |
|---|---|---|
| `coap-client` | libcoap 4.3.5, C, a different project | `real_client_test.rs` |
| `coap` 0.27 | a full Rust UDP client | `e2e_test.rs` |
| `coap-lite` 0.13 | an independent Rust codec (`coap` is built on it) | `e2e_test.rs`, `bounds_test.rs` |

None is `#[ignore]`d, none has a skip-when-missing gate — `real_client_test.rs::require_coap_client`
**fails** when libcoap is absent — and both crates are **unconditional** dev-dependencies
(`[dev-dependencies] coap-lite = "0.13"`, `coap = "0.27"`; neither is `optional = true`), so the
evidence compiles wherever the suite does. (The line also said "Experimental" once while the
code said `Beta` — the code was right.) The blocking CI `test` job runs
`tcp,http,dns,udp,redis,mcp-stdio` only and so does not execute any of it; run it yourself.

**RFC**: RFC 7252 (Constrained Application Protocol)
**Port**: 5683, declared as `PrivilegeRequirement::None`
**Feature**: `coap` (no optional dependencies — the codec is hand-rolled)

5683 is above 1023, so a `PrivilegedPort` declaration here could never fire and would read as
protection that is really dead code. `None` is the honest declaration.

## Library choice

**Hand-rolled**, in `codec.rs` (~650 lines; this said "~470" when the file was already 560, and
line counts in prose are worth distrusting for exactly that reason). `coap-lite` is a good codec
and would have worked,
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
- 4.13 Request Entity Too Large (§5.9.2.9) for a datagram over `codec::MAX_MESSAGE_LEN` — see
  decision 6b.

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
| `refused_too_large` | over `MAX_MESSAGE_LEN`; refused unread, never a prompt (decision 6b) | WARN |
| `fail_closed_llm_error` | the LLM backend failed | **ERROR** |
| `fail_closed_no_action` | the model was asked and returned nothing usable | **ERROR** |

`grep 'decision=fail_closed_'` finds every request the model did not actually answer. The
error text stays in the log and never reaches the wire — only the code does.

`tests/server/coap/llm_failure_test.rs` drives both fail-closed rows from a socket and asserts
that the two tags are **different**, which is the whole claim: the peer receives the same five
bytes either way, and a backend outage and a model answering badly are opposite problems with
opposite fixes. It also asserts the refusal is *matchable* — ACK, the request's message id, the
request's token — because a 5.03 a client discards as unsolicited is the silence this path
exists to replace. Until September 2026 this table was seven rows that nothing checked.

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

### 6b. …and a request has to fit one too, which is the bound `max_inbound_bytes` declares

`codec::MAX_MESSAGE_LEN` is **1152 bytes** — RFC 7252 §4.6's `MAX_MESSAGE_SIZE`, the number
1024 is derived *from*. `handle_datagram` refuses a larger datagram **before decoding it and
before any model call**, answering 4.13 Request Entity Too Large (§5.9.2.9) for a request and a
Reset for a Confirmable non-request. The refusal carries the request's own message id and token,
read out of `codec::message_prefix` — a fixed-offset read that walks no options — because CoAP
matches a reply to its request by token equality and a refusal the client discards is silence
wearing a response code.

**This is a correction, not a feature.** `metadata()` declared
`.max_inbound_bytes(MAX_PAYLOAD_LEN)`, i.e. 1024 — and `MAX_PAYLOAD_LEN` bounds what the server
*writes*. Nothing on the inbound path checked it or anything else: a 2000-byte request was
decoded, turned into an event and handed to the model. The declaration named a number the code
never enforced, which is precisely what `max_inbound_bytes`' own doc comment warns against
("the number must be the one the code enforces, at the point the length is decided"). Nothing
caught it because `tests/max_inbound_bytes_bound_plus_one_test.rs`, the generic `bound + 1`
probe, reaches servers over `TcpStream::connect` and skips every UDP protocol.

Two details that look like slack and are not:

- **The receive buffer stays at 2048, deliberately larger than the bound.** `recv_from`
  discards whatever does not fit, so a buffer sized *at* 1152 would deliver a 5000-byte
  datagram as a legal-looking 1152-byte one and the guard could never fire.
- **`decision=refused_too_large`** is its own log token, not `fail_closed_`: nothing failed,
  the server decided. What it marks is a request that never became a prompt.

`tests/server/coap/bounds_test.rs::test_inbound_message_bound_refuses_one_byte_over_without_asking_the_model`
drives it from a socket — 1152 bytes served, 1153 refused 4.13 — and asserts the model-call
count directly, so the zero is the bound's and not the routing table's.

### 7. `encode` refuses what `decode` would refuse, instead of truncating

`CoapMessage::encode` returns `Result<Vec<u8>, EncodeError>`. It used to return `Vec<u8>` and
write `self.token.len().min(8)` as the TKL while copying only those eight octets, so a token
longer than the format allows reached the wire *shortened* — a perfectly parseable message
carrying a token that is not the one that was asked for. `decode` refuses `tkl > 8`, so the two
directions disagreed about what is legal, silently.

Truncation is the worst available outcome on this protocol specifically. **CoAP's whole
request/response matching is token equality** (RFC 7252 §5.3.2): a client that sent a 9-octet
token and receives an 8-octet one discards the reply as unsolicited, so the server logs a
successful send, the client logs nothing at all, and the request times out. The same change
bounds an option value at [`MAX_OPTION_LEN`], which was narrowed by an `as u16`.

Nothing in NetGet could reach either today — a response's token is echoed from the request and
`decode` has already bounded that at 8, and the model never sees the token — so both were
latent in a `pub fn` rather than reachable from the wire. `mod.rs::send` answers the refusal
with `decision=fail_closed_encode` and sends nothing, which is the same fail-closed vocabulary
the rest of the protocol uses.

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

1. **No Observe, no Block-wise.** Every message therefore has to fit one datagram, in both
   directions: a request is capped at `codec::MAX_MESSAGE_LEN` (1152, decision 6b) and a
   response payload at `codec::MAX_PAYLOAD_LEN` (1024, decision 6). This entry used to say that
   an over-large request "is truncated by the kernel and then fails to decode, which produces an
   RST for a CON" — that was the behaviour when nothing bounded the inbound path; it is now a
   4.13 decided from the length, and truncation cannot disguise it because the buffer is larger
   than the bound.
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

## Maturity: the six conditions

The root `CLAUDE.md` defines `Stable` as six conditions. Re-derived against source on
16 September 2026 rather than inherited. All six hold, one of them only after a repair:

| # | condition | holds? |
|---|---|---|
| 1 | two independent third-party clients, no skip, no `#[ignore]` | **yes** — libcoap's `coap-client` (C) and the `coap`/`coap-lite` crates; `require_coap_client` hard-fails |
| 2 | the pcap oracle is green over its wire traffic | **yes** — `e2e_test.rs::exchange` runs `PcapOracle::udp("coap")` over every datagram in both directions |
| 3 | a fuzz target exists and has run clean, with a corpus | **yes, as of this pass** — 292,391 runs in 91s, clean; see below |
| 4 | every declared bound has a test | **yes, as of this pass** — `bounds_test.rs`, each verified by removal |
| 5 | both `CLAUDE.md` files verified against source in this pass | **yes** — this file and `tests/server/coap/CLAUDE.md` |
| 6 | no `#[ignore]`, no skip-when-missing gate | **yes** — `grep -rn '#\[ignore\]' tests/server/coap/` is empty |

**Condition 3 was false until this pass, and how it broke is the useful part.**
`fuzz/fuzz_targets/coap_message.rs` was written on 15 September 2026 against an `encode` that
returned `Vec<u8>`; `0996d00f`, the same day, made it return `Result<Vec<u8>, EncodeError>` so
an over-long token is refused rather than truncated. The target's
`CoapMessage::decode(&reencoded)` has been an `E0308` ever since. Nothing noticed: `fuzz/` is
deliberately its own workspace (it needs nightly and links libFuzzer's runtime), so `cargo
check` at the repository root never compiles it and **no CI job builds it at all**. A fuzz
target that does not compile has not run, so "a fuzz target exists and has run clean" was a
claim about a file rather than about an execution. Rebuild the targets when you change any
decoder they touch:

```bash
cd fuzz && rustup run nightly-2025-12-04 cargo fuzz build coap_message
```

(`cargo +nightly fuzz` does not work on this machine — asdf's shims sit ahead of
`~/.cargo/bin` on `PATH`, so `cargo` is not the rustup proxy and `+toolchain` is read as a
subcommand name. `rustup run <toolchain> cargo fuzz …` is the form that works.)

The corpus has **no depth bomb**, and that is deliberate rather than a gap: `CoapMessage::decode`
is a `while` loop and `read_extended` does not recurse, so there is no nesting class for one to
reach. The equivalent blind spot is the option delta/length *extension* encodings — where every
length a peer can state lives — and `option_delta_ext13` / `option_delta_ext14` seed those. The
condition's letter is inapplicable; its reason is satisfied.

### What Stable does *not* mean here

It means the evidence for the surface this server implements is complete. The surface is a
subset of RFC 7252: **no Observe (RFC 7641), no Block-wise transfer (RFC 7959), no DTLS/CoAPS,
no separate responses, no deduplication cache.** All three peers drive the same one-datagram
request/response shape because it is the only shape there is. An IoT client that needs Observe
cannot use this server, and nothing here says otherwise.

The `openvpn` precedent is the one to check this against, and it does not apply: openvpn stays
Experimental because it implements only the front of the protocol, so *no client can use it for
what the protocol is for*. A CoAP client can use this for what CoAP is for — libcoap did, in
both directions, with the bytes asserted.

## References

- [RFC 7252: The Constrained Application Protocol (CoAP)](https://datatracker.ietf.org/doc/html/rfc7252)
- [RFC 7641: Observing Resources in CoAP](https://datatracker.ietf.org/doc/html/rfc7641) — not implemented
- [RFC 7959: Block-Wise Transfers in CoAP](https://datatracker.ietf.org/doc/html/rfc7959) — not implemented
- [coap-lite](https://docs.rs/coap-lite) / [coap](https://docs.rs/coap) — the test peers, not runtime dependencies
