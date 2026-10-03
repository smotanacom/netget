# tests/server/torrent_peer

BitTorrent Peer Wire Protocol (BEP 3) **server**. Every peer in this suite is a raw
`tokio::net::TcpStream` with the handshake and length-prefixed frames built by hand in the
test. There is **no third-party BitTorrent client** anywhere — no transmission, no aria2,
nothing to install. Hand-built frames are an independent *reading* of BEP 3, not an
independent implementation, which is why `src/server/torrent_peer` is
`DevelopmentState::Experimental` and must stay there.

## Files

### `e2e_test.rs` — two tests, **6 LLM calls**

Mocked NetGet subprocess, `.expect_calls(1)` on every rule.

| test | calls | rules |
|---|---|---|
| `test_peer_handshake_and_bitfield` | 3 | startup, `peer_handshake` → `send_handshake`, `peer_bitfield_message` → `send_bitfield` + `send_unchoke` |
| `test_peer_piece_request` | 4 | startup, `peer_handshake` → `send_handshake` + `send_unchoke`, `peer_choke_message` → `send_unchoke`, `peer_request_message` → `send_piece` |

The piece rule uses `respond_with_actions_from_event` to echo the requested `index`/`begin`
back, so the reply matches whatever block the test asked for. The block itself goes out as
`block_hex` (`"48656c6c6f20576f726c64"` = "Hello World") — the protocol's declared encoding
field, not raw bytes stuffed into a text parameter.

### `peer_inject_test.rs` — one in-process test, **0 LLM calls**

`injected_peer_message_reaches_raw_peer_and_close_sends_eof`. Uses `netget::` APIs directly.
The server answers the handshake through a static handler and the peer is a raw tokio socket,
so the bytes are asserted on the wire. Also proves the connection counters move (what the
dashboard rail's `↓ ↑` reads) and that `close_connection` half-closes and releases the peer
handle.

### `llm_failure_test.rs` — one in-process test, **0 LLM calls** (the backend is a closed port)

`llm_failure_chokes_the_peer_and_closes_without_leaking_the_error`. The backend URL points at
`127.0.0.1:1`, so every `call_llm` errors.

BEP 3 has no error message and no free-text field, so a backend failure can only be expressed
with the protocol's own refusal: `choke` (`00 00 00 01 00`). The test asserts the peer
receives exactly that frame followed by EOF, with **nothing else on the wire** — in particular
nothing derived from the backend error. This is the `WireFailure` rule in its most constrained
form: where the wire cannot carry a category, it carries none, and the detail goes to the log.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features torrent-peer \
    --test server -- server::torrent_peer --test-threads=100
```

No Ollama: `e2e_test` is mocked and the other two deliberately point the LLM nowhere.

## `connection_bounds_test.rs` — 3 in-process tests, **0 LLM calls** (the backend is a dead port)

The read deadlines in `src/server/torrent_peer/mod.rs` — `HANDSHAKE_READ_TIMEOUT` (300s) and `IDLE_AFTER_HANDSHAKE_TIMEOUT` (180s) — driven from the wire. No
mock: these assert on *clocks*, not on answers, and a reachable backend would only add noise.
Loopback only.

**What each test is for.** A peer that has connected and sent no handshake must eventually be
let go of, because nothing else in the process will close that socket — it holds a task, an
`AppState` row and one of `MAX_CONNECTIONS` slots before it has identified itself with so much
as an info-hash. The two bounds are different claims, so the second test completes a hand-built
BEP 3 handshake, has it answered by a static rule, and then goes quiet, which must be governed
by `idle_timeout_secs` rather than by the first-byte one. (There is no parked-for-a-human test
here as there is for `ftp` and `telnet`: the deadline wraps this protocol's `read()` and nothing
else, and the LLM round-trip happens after it has already returned.)

**The last test is the regression, and it is deliberately the slow one.** The first-byte bound
was 30 seconds, and NetGet's own BitTorrent peer client is precisely a peer that bound stranded:
`src/client/torrent_peer/mod.rs` blocks on `read_exact` for the *other* peer's 68-byte handshake and sends its own only when the `send_handshake` action says to, so **both ends wait**, and a client made from the dashboard is routed `*` → manual — so it connects and
waits for a person, who gets 300 seconds (`src/state/intercepts.rs`). It is now 300s. Proving
that means holding a silent peer open **past 30 seconds with no startup parameters passed at
all**, so the wait cannot be made cheaper than the claim.

Every other test passes a short override instead of waiting the default out, which is also what
proves `first_byte_timeout_secs` and `idle_timeout_secs` are read rather than merely declared:
a parameter that was ignored would leave the 300-second default in force and the test would time
out. Each bound was verified by removing it and watching its test fail, and the default was
verified by putting 30 back and watching the regression test fail.
