# Nostr relay tests

Run everything:

```bash
./cargo-isolated.sh test --no-default-features --features nostr --test server -- nostr:: --test-threads=100
```

## Strategy

The evidence is `real_client_test.rs`, which points two clients NetGet did not write at the
relay: **nak** (fiatjaf's Go client on go-nostr and coder/websocket — `brew install nak`, or the
release binary on Linux) and **rust-nostr** through its Python bindings (`pip install
nostr-sdk`). Both fail, never skip, when absent. nak shares nothing with NetGet, so it carries
the WebSocket-framing evidence too; rust-nostr's WebSocket layer is tungstenite-based like the
server's, so it is protocol evidence only.

Everything else reads bytes NetGet wrote, with a raw tokio-tungstenite peer (`common::Peer`),
and exists for what the clients never send: tampered events, malformed messages, bounds,
failure paths, the model-call count. Most suites start the relay **in process** through
`ServerForm` (the dashboard's path) with static, script or manual handlers and a dead model
endpoint, so they are deterministic and cost no model call. `e2e_test.rs` and one case in
`real_client_test.rs` use the spawned binary with the mock model.

## Files

| File | What it proves | Model calls |
|---|---|---|
| `common.rs` | helpers: in-process relay, the raw WebSocket `Peer`, events signed in the test (`note`), `relay_handlers`, `require_tool`/`run_nak` (stdin is null: nak reads a filter or event from a non-terminal stdin and waits), the recording TCP relay for the pcap oracle | — |
| `real_client_test.rs` | nak publishes and reads `success`, and the pcap oracle reads the recorded session with both directions dissected as `websocket`; nak reads a rejection's reason; `nak req` receives exactly the events the filters allow (kind 7 and a note older than `since` dropped by NetGet, `-l 1` the newest, `-t t=film`, `-a <someone else>` nothing), verifies each itself and each passes `nak verify`, all signed by the pinned relay key; `nak relay` reads NIP-11 (`name`, `description`, `supported_nips`, `self`, `limitation`); a streaming `nak req` receives an event another nak published, byte for byte with its author's signature; rust-nostr publishes (its `SendEventOutput.success` names the relay) and fetches the three kind-1 events; one case with a **mocked model** behind nak | 3 (mocked case) |
| `e2e_test.rs` | one relay, mocked model: accept → `OK true ""`; reject with a bare reason → `OK false "blocked: …"`; a tampered event refused `invalid:` with no model call; a REQ answered with three events, the kind 7 dropped, each verified, then `EOSE`; a refused REQ → `CLOSED "restricted: …"` and no `EOSE`; `CLOSE` with no model call; live delivery to a second connection's subscription, and nothing to a closed one. `verify_mocks` pins the counts | 7 |
| `wire_test.rs` | the three nak-signed vectors verify; the measured escaping disagreement (nak signs U+0008/U+000C/U+0001 the go-nostr way and under neither other form) and events signed under all three forms verify; changed content, recomputed id with the old signature, a flipped signature byte, uppercase hex, and five malformed fields each refused with their own decision; the relay key round-trips; filter matching, `limit` newest-first, ORed filters, extension keys; malformed filters; message parsing and every mechanical refusal; depth bombs of 129 and 60 000 levels are a `NOTICE`; reason prefixes; the model's events read leniently, their contested controls dropped, 501 refused | 0 |
| `connection_bounds_test.rs` | 256 upgrades admitted, the 257th `503` + `Retry-After`, one slot back per close; a half-sent head `408` after `handshake_timeout_secs`; with `idle_timeout_secs: 2` a peer that never reads gets a Ping then close `1001` while a tungstenite peer that answers Pings is still served past the bound; exactly `MAX_MESSAGE_BYTES` read, one more closed `1009`; 20 subscriptions then `CLOSED rate-limited:`, an id reused replaces, `CLOSE` frees a slot; 11 filters and a 65-character id refused with their own messages, 10 and 64 accepted; a 60 000-level depth bomb over the wire, then the connection still served; a `manual`-parked event with 64 queued behind it, the 65th refused `rate-limited:` | 0 |
| `llm_failure_test.rs` | dead backend → `OK false error:` (or `rate-limited:`) for an event and `CLOSED` for a REQ, no leaked error text, `decision=fail_closed_llm_error`; an empty handler → `OK false` + `model_silent`, and an empty REQ answer → `EOSE`; events offered as the answer to an event, and an event with no kind as the answer to a REQ → `fail_closed_bad_action` | 0 (the failing one never reaches a model) |
| `peer_inject_test.rs` | `send_to_peer`: a `NOTICE`; `send_nostr_events` into an open subscription, signed by the relay and held to its filters; `close_nostr_subscription` → `CLOSED`; `close_connection` → `Disconnected` and a close frame `1000` | 0 |
| `answer_with_test.rs` | the event hint names both decisions as literal actions; the REQ hint puts kinds, tags, `since`, `limit` and ORed filters into words, and says out loud when an `authors` or `ids` filter can match nothing the relay signs; the examples are placeholders | 0 |

The stop-server case (a half-sent request head) is in `tests/stop_server_stops_connections_test.rs`;
the eval cases and their probe checks are in `tests/eval/`.

## How each guard was shown to matter

Removed, the tests run, then restored:

| Guard removed | Test that failed |
|---|---|
| connection cap (limiter to 1 000 000) | `the_connection_cap…` (the 257th got `101`) |
| request-head deadline (→ 3600 s) | `a_peer_that_sends_no_request_head…` (no `408` in 10 s) |
| idle bound (→ 3600 s) | `a_peer_that_never_answers_a_ping…` (never closed) |
| message size (→ 64 MiB) | `max_message_bytes…` (the over-size message answered `NOTICE`) |
| subscription cap | `subscriptions_filters_and_ids…` (the 21st got `EOSE`) |
| queue bound (→ 100 000) | `past_the_queue…` (no refusal) |
| subscription id length | `subscriptions_filters_and_ids…` — after a fix: the first run passed because the connection was already at its subscription cap and the long id was refused as `rate-limited:`; the test now frees room first and asserts each refusal's own message |
| id recomputation | `a_changed_field…`, `client_messages_parse…` |
| filter enforcement (send every supplied event) | `nak_req_receives_exactly…`, `rust_nostr_publishes_and_fetches` (rust-nostr keeps what it is sent), e2e, `peer_inject` |
| filter count | `client_messages_parse…`, `subscriptions_filters_and_ids…` |
| signature check | `a_changed_field…` (the re-hashed event with the old signature accepted) |

The depth bound is `serde_json`'s recursion limit and cannot be removed without its
`unbounded_depth` feature; the two depth tests show the refusal and the connection surviving.

## Notes

- nak 0.20.7 and nostr-sdk 0.45.1 were installed with Homebrew and pip on the machine these were
  written on; CI's `registry-audit` installs the same versions and runs
  `nostr::real_client_test` in its evidence loop.
- The pcap oracle needs `tshark` and fails without it (`tests/helpers/pcap_oracle.rs`).
