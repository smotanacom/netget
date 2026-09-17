# tests/server/torrent_tracker

BitTorrent HTTP tracker (BEP 3 announce/scrape) **server**.

**`real_client_test.rs` drives the real `aria2c`, and the protocol is `Beta` as of
September 2026.** This page said the opposite for a long time — "there is no third-party
BitTorrent client anywhere … which is why it is Experimental and must stay there" — and that
was true when written and stopped being true the moment aria2 was installed. The rest of the
suite still writes its own HTTP/1.1 GETs onto a raw `tokio::net::TcpStream` and decodes the
replies with `serde_bencode`, which is the crate the *server encodes with*: one crate
round-tripping through itself, plus an independent *reading* of BEP 3. That is worth keeping
for the cases aria2 will not produce, but it is not what the rating rests on.

## Files

### `e2e_test.rs` — three tests, **5 LLM calls**

| test | calls | rules |
|---|---|---|
| `test_tracker_announce_and_scrape` | 3 | startup, `tracker_announce_request` → `send_announce_response` (two peers), `tracker_scrape_request` → `send_scrape_response` |
| `test_tracker_error_response` | 2 | startup, `tracker_announce_request` → `send_error_response` |
| `tracker_connection_stats_are_recorded` | 0 | — |

The third is in-process (uses `netget::` APIs directly rather than spawning the binary) and
answers every announce with a `*` **static** handler, so no LLM call fires. Its point: the
server must call `update_connection_stats`, or the rail shows `↓0 ↑0` and a stale
`last_activity`.

### `peer_inject_test.rs` — two tests, **0 LLM calls**

The dashboard's `[ message this peer ]` / `[ disconnect this peer ]` path. Both are in-process,
both pass `instruction: Some(String::new())` — `ServerForm::create` substitutes a default
instruction for `None`, and that alone makes a server consult the model — and both answer with
a `*` static handler, so the LLM is never reached.

| test | asserts |
|---|---|
| `injected_tracker_action_reaches_raw_socket_and_close_sends_eof` | the handle exists **before the peer has written a byte** (the parked-announce window); an injected `send_announce_response` returns `Sent` and its bytes arrive as a real HTTP 200 carrying the injected interval; the write is counted in `bytes_sent`; `close_connection` returns `Disconnected` and the socket reads EOF; the handle is gone afterwards |
| `announce_still_answered_and_the_connection_entry_is_closed` | the protocol's own path is unaffected, **and** the connection entry stops being `Active` when the exchange ends — which it never did before the handle was adopted |

### `llm_failure_test.rs` — one test, **1 LLM call**

`tracker_answers_a_category_when_the_llm_fails`. Only the *startup* instruction is mocked, so
`tracker_announce_request` matches no rule, the mock answers HTTP 500, and `call_llm` returns
`Err` — the same shape as a real backend outage.

Three separate assertions, each a distinct defect if it regresses:

1. A **complete** HTTP response arrives, with `Content-Length` and `Connection: close`. The
   old answer was a bare `HTTP/1.1 500 Internal Server Error\r\n\r\n` with no body.
2. The body is a bencoded `failure reason` — the one refusal BEP 3 defines and the only thing
   a BitTorrent client displays.
3. That text carries nothing from the error: no backend URL, model name, path or `anyhow`
   chain. It is the fixed `WireFailure` category.

It also waits for `decision=fail_closed_llm_error` in the log, which distinguishes the
LLM-error path from a model that answered with `send_error_response`.

Silence is wrong here for the same reason as DHT: a client waits for a peer list, retries, and
eventually marks the tracker dead.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features torrent-tracker \
    --test server -- server::torrent_tracker --test-threads=100
```

All mocked; no Ollama and nothing serializing requests behind a lock. Helpers come from
`tests/helpers/` via `tests/server/helpers.rs`, which is a one-line re-export — there is no
`start_server_with_instruction()` or `wait_for_server_ready()`; use `start_netget_server` plus
`wait_for_mocks` / `wait_for_any`.

## `real_client_test.rs` — two tests, **4 LLM calls**, real `aria2c` 1.37.0

The evidence the rest of this directory cannot produce. Both tests **fail** (naming
`brew install aria2`) when the binary is absent; neither is `#[ignore]`d.

| test | what aria2 proves |
|---|---|
| `test_tracker_compact_peers_are_decoded_by_real_aria2c` | it dials **both** `10.0.0.1:6881` and `10.0.0.2:6882` after decoding our compact peer string |
| `test_tracker_failure_reason_is_read_back_by_real_aria2c` | it prints `Tracker returned failure reason: <our text>`, i.e. its own parse of our bencoded refusal dict inside an HTTP 200 |

Three things make this evidence rather than a liveness check:

- **`compact=1` is hardcoded in aria2's announce format string.** It never requests the
  dictionary form, so the compact encoder — a bencode byte string, six bytes per peer, four
  address octets then a big-endian port, nothing self-describing anywhere in it — is the branch
  a real swarm always takes, and it is the branch no earlier test drove against a real client.
  Asserting **both** peers is what catches a stride or length error, which would lose exactly
  one of them.
- **The mock rule matches on `info_hash` AND `compact`.** That is the server-side half: if the
  percent-decode or hex-encode is wrong, or `compact` does not arrive as 1, the rule never
  fires, the tracker answers nothing, and the failure shows up in both halves at once instead of
  passing quietly.
- **DHT, DHT6 and LPD are all disabled.** Otherwise a peer aria2 found elsewhere would be
  indistinguishable from one we returned and the assertion would be vacuous.

aria2 is expected to exit non-zero: the magnet names a swarm that does not exist, so after
announcing it fails to fetch metadata from two unreachable peers. The download is not the point
and the exit status is deliberately not asserted.

**Still not covered by a real client**: the dictionary peer form (aria2 parses it but never asks
for it), IPv6/BEP 7 `peers6` (not implemented), `/scrape` (aria2 does not issue one on its own),
multi-`info_hash` scrape, and the 400/408 paths.
