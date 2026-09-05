# STOMP E2E Testing

Three files, **22 tests, 10 LLM calls**.

| File | Tests | LLM calls | Peer | What it is for |
|---|---|---|---|---|
| `e2e_test.rs` | 2 | 7 | `async-stomp` 0.6.3 | **the Beta evidence** |
| `raw_socket_test.rs` | 2 | 3 | raw socket | what a client library cannot express |
| `codec_test.rs` | 18 | 0 | none | the codec against spec byte literals |

## `e2e_test.rs` — the real client, and why it counts

The peer is `async-stomp` 0.6.3: an independent STOMP 1.2 implementation, not a codec this test
drives frame by frame. `Connector::connect()` opens the socket, sends `CONNECT`, and **refuses
to return a transport unless the reply is a well-formed `CONNECTED` carrying a `version`
header**; every later frame is decoded by its own parser into typed values
(`FromServer::Message`, `FromServer::Receipt`), hard-erroring on a missing required header.

It clears each clause of the Beta bar in the root `CLAUDE.md`:

- **Not `#[ignore]`d** — runs in the default suite.
- **Cannot skip.** `async-stomp` is a compiled-in crate dependency; there is no "is it
  installed?" question and no `SKIP: … not installed` branch. That silent-pass shape is what
  keeps `kubernetes`, `oci_registry`, `maven` and `websocket` at Experimental. Every failure
  path here is an `assert!`, an `expect`, or a `?` that propagates — a timeout waiting for a
  frame **panics**, it does not return `Ok(())`.
- **Not circular.** Nothing in the file touches `netget::server::stomp::frame`. A test that
  parsed the server's output with the server's own parser would assert only that one module
  round-trips through itself — the `rss`/`webrtc_signaling` trap.

### The negative control, which is the part worth keeping

Deleting the `version` header from `CONNECTED` in `actions.rs` was tried, and:

- `test_stomp_session_against_the_async_stomp_client` **FAILED** — `async-stomp` rejected the
  handshake, as a real client would.
- `raw_socket_test.rs` still **passed** — the hand-written peer never checked `version`.

That is the concrete demonstration that the real client is strictly stronger evidence, and the
reason the rating rests on `e2e_test.rs` alone. Re-run it before trusting any future change to
this suite: a green run proves nothing unless a broken server turns it red.

### The two tests

| Test | Calls | What it pins |
|---|---|---|
| `test_stomp_session_against_the_async_stomp_client` | 5 | startup + `stomp_connect` + `stomp_subscribe` + `stomp_send` + `stomp_disconnect` |
| `test_stomp_connect_refusal_is_seen_by_the_async_stomp_client` | 2 | startup + `stomp_connect` |

The first runs a whole session on one connection: the handshake; a `SUBSCRIBE` answered with a
`MESSAGE` whose `subscription` is asserted to be the id **the client chose** (a `MESSAGE` naming
any other id is discarded by a real client, so a hardcoded one would make the test prove less
than it looks); a `SEND` echoed back; and a `DISCONNECT` whose `RECEIPT` is followed by the
stream ending rather than erroring.

The second asserts that a refusal reaches a real client as a *decodable* `ERROR` rather than a
dropped connection — `async-stomp` fails the handshake with the frame it received, so asserting
on its error text proves both the `message` header and the body survived its parser.

## `raw_socket_test.rs` — only what `async-stomp` cannot do

Two hard limits in `async-stomp`, both verified against its source, define this file's scope:

- **It never writes `content-length`** (`ToServer::Send` builds the frame without one), so it
  physically cannot publish a body containing NUL — exactly the case where `content-length`
  becomes authoritative and a "read to the terminator" parser truncates.
- **`Connector::connect()` always handshakes first and always sends `accept-version:1.2`**, so
  a frame before the handshake, a refused version, and deliberately broken framing are all
  unreachable through it.

| Test | Calls | Notes |
|---|---|---|
| `test_stomp_binary_body_survives_the_encoding_round_trip` | 2 | the handshake is answered by a **static** handler declared on the server, so it costs no LLM call — and exercises the deterministic path an operator would actually use |
| `test_stomp_protocol_errors_never_reach_the_model` | 1 | startup only, and **that is the assertion** |

In the second, the mock has *only* a startup rule across three connections. Any event that
reached the model would match no rule, fall through to a real LLM call, and fail —
`verify_mocks` is what makes the absence assertable. This is the check that matters most for a
honeypot: if framing were a question the model got asked, a stranger could provoke an LLM round
trip with one malformed byte.

This file is **not** evidence for the maturity rating. A hand-written reader is an independent
*reading* of the spec, not an independent implementation — the `dhcp`/`usb-serial` class. It
also avoids `netget::server::stomp::frame`, for the same reason `e2e_test.rs` does.

## `codec_test.rs` — spec literals, no socket, no model

Assertions are against **byte literals written from the spec**, not against whatever the encoder
happens to produce. `encode`→`parse` agreeing proves nothing on its own, so the literals are the
anchor and the round-trip tests sit on top of them.

- a body containing NUL under `content-length`, and a `content-length` not followed by NUL
- `\r\n` line endings; heart-beat EOLs; leading EOLs before a frame
- **every** prefix of a frame returns `Incomplete` and consumes nothing (a loop over all split
  points — "works when the whole frame arrives in one read" is a different, weaker claim)
- the four escape sequences and nothing else; an undefined escape is fatal at both the header
  and the frame level
- the `CONNECT`/`STOMP`/`CONNECTED` escaping exemption, in both directions — getting it
  backwards corrupts every `host` header containing a colon
- an unterminated frame past `MAX_FRAME_BYTES`; first-occurrence-wins for a repeated header

## Mock rules

Every rule is on a distinct event id, so the first-match-wins trap does not apply. Two use
`respond_with_actions_from_event` because they *must*:

- `stomp_subscribe` quotes back `e["id"]` — see above.
- `stomp_send` echoes `e["body"]` **with** `e["body_encoding"]`. In `raw_socket_test.rs` the
  published body is binary (`00 01 ff 00 7f 80`), so the bytes only come back intact if the hex
  contract holds in both directions. A static mock could not express this.

## Privacy

127.0.0.1 only, ephemeral ports, no external endpoints, no real broker.

## Test execution

```bash
./cargo-isolated.sh test --no-default-features --features stomp \
    --test server -- --test-threads=100 stomp
```

## Not covered

Heart-beating, transactions, subscription bookkeeping, STOMP 1.0/1.1 and TLS are not
implemented; see `src/server/stomp/CLAUDE.md`. Also not covered: interop with the brokers' own
client stacks (ActiveMQ/RabbitMQ STOMP), and concurrent sessions. Those are what a human should
check before this goes past Beta.
