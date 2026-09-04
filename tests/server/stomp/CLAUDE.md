# STOMP E2E Testing

Two files, **21 tests, 8 LLM calls total**.

| File | Tests | LLM calls | Needs a socket? |
|---|---|---|---|
| `codec_test.rs` | 18 | 0 | no |
| `e2e_test.rs` | 3 | 8 | yes |

## The peer is hand-written, and that is the whole story about the rating

`e2e_test.rs` drives the server with a raw `TcpStream` and a frame reader written out in the
test file. It is **not** a third-party STOMP client, so nothing here supports a `Beta` rating —
see `src/server/stomp/CLAUDE.md` for the crate that would (`async-stomp 0.6.3`, EUPL-1.2, not
added because `Cargo.toml` was single-writer).

Two deliberate choices in that reader:

- **It does not call `netget::server::stomp::frame::parse_frame`.** A test that parses the
  server's output with the server's own parser asserts only that one module round-trips through
  itself — the circular-evidence trap the root `CLAUDE.md` records for `rss` and
  `webrtc_signaling`, and which `tests/server/websocket/e2e_test.rs` states in its own header.
- **It honours `content-length` and falls back to the NUL terminator.** The server sets
  `content-length` on every non-empty body, so a reader that only scanned for NUL would truncate
  the binary echo below and the test would pass for the wrong reason.

`codec_test.rs` asserts against **byte literals written from the spec**, not against whatever
the encoder happens to produce. That is the point: `encode`→`parse` agreeing proves nothing on
its own, so the literals are the anchor and the round-trip tests sit on top of them.

## LLM call budget

| Test | Calls | Breakdown |
|---|---|---|
| `test_stomp_session_handshake_subscribe_publish_disconnect` | 5 | startup + `stomp_connect` + `stomp_subscribe` + `stomp_send` + `stomp_disconnect` |
| `test_stomp_connect_refused_with_error_frame` | 2 | startup + `stomp_connect` |
| `test_stomp_protocol_errors_never_reach_the_model` | 1 | startup only — **the assertion is that it stays 1** |

The whole session is one server and one connection deliberately: five calls for a six-frame
exchange rather than a server per scenario.

Every rule is on a distinct event id, so the first-match-wins trap does not apply. Two rules use
`respond_with_actions_from_event` because they *must*:

- `stomp_subscribe` quotes back `e["id"]`. A `MESSAGE` naming any other subscription id is
  discarded by a real client, so hardcoding it would make the test prove less than it appears to.
- `stomp_send` echoes `e["body"]` **with `e["body_encoding"]`**. The published body is binary
  (`00 01 ff 00 7f 80`), so the assertion that the exact bytes come back only holds if the hex
  contract works in both directions. A static mock could not express this.

## What each test pins

### `test_stomp_session_handshake_subscribe_publish_disconnect`

1. `CONNECT` → `CONNECTED` carrying `version:1.2`, the session/server the handler chose, and
   **`heart-beat:0,0` even though the client asked for `10000,10000`** — the server implements
   no heart-beat timer and must not promise one.
2. `SUBSCRIBE` with `receipt` → `MESSAGE` **then** `RECEIPT`, in that order. The spec has the
   receipt acknowledge a frame that has been *processed*; sending it first would also make it
   impossible for a handler to answer before the acknowledgement.
3. `SEND` of a binary body with `receipt` → the same bytes back in a `MESSAGE`, then `RECEIPT`.
4. `DISCONNECT` with `receipt` → `RECEIPT`, then EOF.

### `test_stomp_connect_refused_with_error_frame`

The model answering `send_stomp_error` must reach the wire as an `ERROR` frame **and** close the
connection, which the spec requires of the server whoever produced the error.

### `test_stomp_protocol_errors_never_reach_the_model`

Three connections against one server, and the mock has **only** a startup rule. Any event that
reached the model would match nothing, fall through to a real LLM call, and fail —
`verify_mocks` is what makes the absence assertable.

- a `SUBSCRIBE` before the handshake → `ERROR message:expected CONNECT`
- `accept-version:1.0,1.1` → `ERROR message:unsupported version` carrying `version:1.2`, as the
  spec asks a server to name what it does speak
- a header line with no `:` → `ERROR message:malformed frame`

This is the check that matters most for a honeypot: if framing were a question the model got
asked, a stranger could provoke an LLM round trip with one malformed byte.

## Codec coverage (`codec_test.rs`)

Edges the e2e test cannot reach through a socket:

- a body containing NUL, under `content-length` (the case a "read to the terminator" parser
  silently truncates), and a `content-length` not followed by NUL
- `\r\n` line endings; heart-beat EOLs; leading EOLs before a frame
- **every** prefix of a frame returns `Incomplete` and consumes nothing (a loop over all split
  points, because "works when the whole frame arrives in one read" is not the same claim)
- the four escape sequences and nothing else; an undefined escape is fatal at both the header
  and the frame level
- the `CONNECT`/`STOMP`/`CONNECTED` escaping exemption, asserted in both directions — getting it
  backwards corrupts every `host` header containing a colon
- an unterminated frame past `MAX_FRAME_BYTES`
- first-occurrence-wins for a repeated header

## Privacy

127.0.0.1 only, ephemeral ports, no external endpoints, no real broker.

## Test execution

```bash
./cargo-isolated.sh test --no-default-features --features stomp \
    --test server -- --test-threads=100 stomp
```

## Not covered

Heart-beating, transactions, subscription bookkeeping, STOMP 1.0/1.1, TLS — none of them are
implemented; see `src/server/stomp/CLAUDE.md`. Also not covered: concurrent connections, and
anything an actual STOMP client library would do differently from the reader in this file.
