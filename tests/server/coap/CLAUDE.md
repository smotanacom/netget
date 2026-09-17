# CoAP E2E Tests

## Strategy

Two independent peers, neither of which shares a line of code with
`src/server/coap/codec.rs` (which is hand-rolled precisely so this is true):

1. **`coap` 0.27 (`UdpCoAPClient`)** — a real CoAP client. It does its own CON/ACK matching, so
   a wrong message id or a dropped token shows up as a *timeout*, not as a passing test. It is
   used for the resource-layer assertions: 2.05 Content with a JSON body, 2.04 Changed after a
   POST, 4.04 Not Found, and the Content-Format option.

2. **`coap-lite` 0.13** — an independent codec, used directly over a raw `UdpSocket` to build
   requests with a *chosen* token and message id and to decode the replies field by field. This
   is where the message layer is pinned: ACK type, message-id echo, 8-byte token echo, NON
   answered by NON with a fresh message id, and the RST reply to a CoAP ping.

Plus codec assertions against literal RFC bytes: the four header bits
(`0x40 0x01 0x00 0x01` is Ver=1/CON/TKL=0/GET/MID=1), the code arithmetic (2.05 = 0x45,
4.04 = 0x84), and an option round trip that exercises **both** delta extension forms — nibble
13 (one extra byte) and nibble 14 (two extra bytes) — with coap-lite asked to confirm it can
read what we wrote.

**Nothing here asserts that a datagram arrived.** Every assertion is on a decoded response code,
option, token or payload.

## LLM call budget

| Test | Startup | Events | Total |
|---|---|---|---|
| `test_coap_get_post_and_not_found_with_coap_client` | 1 | 3 | **4** |
| `test_coap_message_layer_echo_and_ping` | 1 | 2 | **3** |
| `test_codec_*` (two tests) | 0 | 0 | **0** |

**Total: 7**, under the ~10 target. The CoAP ping in the second test costs nothing, which is
part of the point: RFC 7252 §4.3 makes that reply mechanical, so it must not reach the model.

## The UDP rule, and how it applies here

`CLAUDE.md` requires UDP-style protocols to use `.respond_with_actions_from_event()` so the
client's random transaction id is echoed dynamically — a static mock with a hardcoded id causes
client timeouts, and the usual "fix" is to weaken the assertion until it passes.

**Every rule in these tests uses `respond_with_actions_from_event()`.** But the thing being
derived dynamically is the **request path and query**, not the message id — because this server
does not make the model handle the message id at all. `codec::response_to` takes the type,
message id and token from the request the server itself parsed; no action parameter carries any
of them. See decision 2 in `src/server/coap/CLAUDE.md` for why.

That removes the hazard the rule exists to prevent (a static mock *cannot* desynchronise an
identifier here) but it also removes the natural assertion, so the echo is pinned explicitly
instead: `test_coap_message_layer_echo_and_ping` builds a request with message id `0x4711` and
token `DE AD BE EF 01 02 03 04` using coap-lite, and asserts both come back byte for byte in an
`Acknowledgement`. A regression that broke the echo would fail that test loudly, and would also
hang the `coap` client in the first test.

## Mock expectations

Four rules in the first test (startup plus one per path), two in the second (startup plus one
rule matched twice, once by the CON request and once by the NON one).

The response generators echo the request's own view of itself — `"{path}?{query} via {mtype}"` —
so the assertion `"/status?verbose=1 via CON"` can only hold if the Uri-Path options, the
Uri-Query option and the message type all decoded correctly and reached the model. A literal
payload would pass just as well against a correct server and would keep passing after a
regression in option parsing.

Both tests end with `server.verify_mocks().await?`.

## Client libraries

Both are dev-dependencies only:

- `coap-lite = "0.13"` — MIT OR Apache-2.0, codec only.
- `coap = "0.27"` — MIT, full UDP client (built on coap-lite).

`UdpCoAPClient::get_with_timeout` / `post_with_timeout` are used rather than the untimed
variants so a broken server fails the test in ten seconds instead of hanging it.

## What is covered

- GET → 2.05 Content with an `application/json` Content-Format, payload intact
- POST with a body → 2.04 Changed; the request body reaches the model and returns in the answer
- GET of an absent resource → 4.04 Not Found, and it does **not** acquire a payload
- CON → piggybacked ACK carrying the request's message id and full 8-byte token
- NON → NON reply with a *different* message id and the same token
- Uri-Path and Uri-Query options decoded and surfaced to the model
- Content-Format encoded as a minimum-length unsigned option and decoded by coap-lite
- Empty CON (CoAP ping) → RST with the ping's message id, with no LLM call
- Header bit layout, code arithmetic, `parse_code_string` including the `"2.05 Content"` form
- Option delta extension forms 13 and 14, round-tripped and confirmed by coap-lite
- Rejection of a short datagram, a wrong version, and a reserved token length

## Coverage gaps

- ~~**No external binary peer.**~~ Closed September 2026: `real_client_test.rs` drives
  libcoap 4.3.5's own `coap-client`, a C implementation — see below. `aiocoap-client` would be
  a fourth and is still not exercised.
- **Observe and Block-wise are not implemented and not tested.** So is DTLS/CoAPS.
- No test for `send_coap_reset` or `ignore_coap_request` as *model* choices — the RST path is
  covered only via the ping, which the server answers itself.
- No test for a retransmitted CON (there is no deduplication cache; a retransmission would
  produce a second LLM call).
- No test for the `"encoding": "hex"` payload path, though `decode_payload` is the same shape as
  the TCP fix and errors legibly on bad hex.
- No test of the 5.03 fail-closed path (LLM error / no usable action).

## Running

```bash
./cargo-isolated.sh test --no-default-features --features coap \
    --test server -- --test-threads=100 coap
```

About one second; everything is loopback UDP and an in-process mock.

## Failure modes seen so far

None across repeated runs. The realistic future flake is the `coap` client's own retransmission
timer firing on a loaded machine while an LLM call is in flight — this server only sends
piggybacked responses, so a slow answer looks like a lost ACK. With mocked LLM calls the answer
is immediate, so it does not arise here; with `--use-ollama` it could.

## The binary peer: libcoap's `coap-client`

`real_client_test.rs::test_coap_get_post_and_not_found_against_libcoap_client` drives
libcoap 4.3.5's `coap-client` — C, by a different project, sharing nothing with this server's
hand-rolled codec or with `coap-lite`. It is **not** `#[ignore]`d and it **fails** rather than
skipping when libcoap is absent.

Three exchanges, each asserted on what libcoap parsed and printed:

| request | answer | asserted |
|---|---|---|
| `GET /sensors/moisture` | 2.05 Content, `application/json` | the exact body, printed on stdout |
| `POST /actuators/valve` with `-e open` | 2.04 Changed | `valve=open` — so the request payload reached the model *and* the response payload came back |
| `GET /nope` | 4.04 Not Found | libcoap reports `4.04`, and the body is asserted **not** to be the moisture representation |

libcoap prints a payload only after accepting version, type, token length, code, message id,
option deltas and the payload marker, and only when the token and message id match the request
it sent. That is what a raw-socket assertion cannot reach.

Two things about driving it:

- **Every invocation passes `-B`,** which bounds libcoap's own retransmission window. The
  server has no deduplication cache, so a retransmitted CON produces a *second* event and a
  second handler call; the mock rules use `expect_at_least` rather than `expect_calls` for
  exactly that reason, and under a loaded runner that is not hypothetical.
- **The 4.04 case is what keeps the other two honest.** Without a negative path, a server that
  answered 2.05 to every request would satisfy both positive assertions.
