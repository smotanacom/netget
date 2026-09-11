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
