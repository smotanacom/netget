# tests/server/torrent_tracker

BitTorrent HTTP tracker (BEP 3 announce/scrape) **server**. Every request in this suite is an
HTTP/1.1 GET written by hand onto a raw `tokio::net::TcpStream`, and every reply is decoded
with `serde_bencode`. There is **no third-party BitTorrent client** anywhere — no
transmission, no aria2, nothing to install. That is an independent *reading* of the spec, not
an independent implementation, which is why `src/server/torrent_tracker` is
`DevelopmentState::Experimental` and must stay there.

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
