# BGP Client E2E Testing

## Strategy

A mocked NetGet **server** speaks BGP to a mocked NetGet **client** over a real socket. Every
assertion is a mock rule that fires only if bytes crossed the wire and parsed — never an
`output_contains` check, which is satisfied by a log line saying the connection failed.

| Test | Asserts | LLM calls |
|---|---|---|
| `test_bgp_client_establishes_session_and_receives_routes` | server's `bgp_open` and `bgp_established` each fire once (client's OPEN and KEEPALIVE arrived); client's `bgp_connected` fires once (server's OPEN and KEEPALIVE arrived); client's `bgp_update_received` fires with `nlri` containing `10.0.0.0/24` | 6 |
| `test_bgp_client_sends_four_octet_asn` | server's `bgp_open` fires with `peer_as` = 4200000000, and the session still reaches Established with `peer_supports_four_octet_as` true | 5 |

**Total: 11 LLM calls, ~6s.** Run:

```bash
CARGO_TARGET_DIR=/tmp/clients-target cargo test --no-default-features --features bgp \
    --test client -- --test-threads=100 bgp
```

## Why the assertions are where they are

`and_event_data_contains("nlri", "10.0.0.0/24")` on the client's `bgp_update_received` is the
regression test for the hex-blob event data. The UPDATE used to reach the handler as
`update_data_hex`, so a rule matching on `nlri` would never have fired.

`and_event_data_contains("peer_as", "4200000000")` on the server's `bgp_open` is the regression
test for the 16-bit truncation. `4200000000 & 0xFFFF` is 60416, so a client that puts
`local_as as u16` on the wire makes this rule miss.

**Mutation-checked**: changing the client's OPEN to `wire::build_open(local_as & 0xFFFF, …)`
made `test_bgp_client_sends_four_octet_asn` fail with
`Rule #1 (event=bgp_open, event_data [("peer_as", "4200000000")]): Expected 1 calls, got 0`.

## What these replaced

Three tests that could not pass and could not fail informatively:

1. Every rule mocked `bgp_open_received` and `bgp_keepalive_received`. Neither event exists on
   the BGP server, whose events are `bgp_open`, `bgp_established`, `bgp_update` and
   `bgp_notification`. No rule ever matched.
2. Client rules answered `bgp_connected` with `send_bgp_open`, a *server* action. The BGP
   client cannot execute it, so `tests/helpers/mock_action_names.rs` panics while the test is
   being configured.
3. Every assertion was `output_contains("BGP") || output_contains("connected")`, satisfied by
   a failure message.

## Notes

- The server binds port 0 (`{AVAILABLE_PORT}` in the prompt, `"port": 0` in the action), so
  the `PrivilegedPort(179)` requirement never fires and the tests need no privilege.
- Client startup parameters go in the `open_client` action's `startup_params`, not in the
  prompt: the mock decides them, the model's wording does not.
- Sleeps are 500ms after each server start and 3s after the client, covering
  OPEN → OPEN → KEEPALIVE → KEEPALIVE → UPDATE with an LLM round trip at three steps.

## `hold_timer_test.rs` — a raw socket peer, no harness

The mock-Ollama harness is the wrong shape for the hold timer twice over: the peer has to go
*deliberately silent*, which a working NetGet server never does, and the assertion is about
specific octets rather than an event firing. So these two drive `BgpClient::connect_with_llm_actions`
directly against a `TcpStream` that speaks BGP by hand.

| Test | Asserts | LLM calls |
|---|---|---|
| `silent_peer_earns_hold_timer_expired_notification_and_close` | after a handshake with a 3s negotiated hold time and total peer silence: a NOTIFICATION arrives whose 21 octets are checked field by field (marker, length 21, type 3, **error code 4**, subcode 0); it took between 1.5s and 10s, so it is the timer and not an unrelated close; at least one keepalive preceded it; the socket then reaches EOF; the client is `Disconnected` in `AppState` | 1, failed |
| `keepaliving_peer_is_not_dropped` | a peer sending a KEEPALIVE every second for 7s — more than two hold times — receives no NOTIFICATION, is never closed on, gets at least 4 keepalives back, and the client is still `Connected` | 1, failed |

**~7s wall clock, both in parallel.** No Ollama: the model name is pinned in `AppState` so
`ensure_model_selected` cannot probe `localhost:11434`, and the endpoint is `127.0.0.1:1`, so
the one call each test makes (on `bgp_connected`) fails with connection-refused and is logged.
That the hold timer fires anyway, while the read loop is off in a failing LLM call, is part of
what is being tested.

**Mutation-checked**, three ways:

| Mutation | Result |
|---|---|
| expiry branch disabled (`if false && silent >= hold`) | `silent_peer_…` fails at the 12s bound; `keepaliving_…` still passes |
| hold timer never reset (`last_received.store` removed) | `keepaliving_…` fails at 3s with the NOTIFICATION octets in the message; `silent_peer_…` still passes |
| `tokio::select!` replaced by a plain `read_exact` | `silent_peer_…` fails on "sent NOTIFICATION 4/0 but kept the connection open" — the NOTIFICATION goes out but the parked read is never preempted |

The first mutation also caught a defect in the test itself: a per-read 12s timeout is reset by
every keepalive, so a client that keepalives forever hung the test instead of failing it. The
deadline is now for the whole wait.

## `update_reply_test.rs` — a handler's answer reaching the wire

Same raw-socket shape as `hold_timer_test.rs`, for the same two reasons: the assertion is about
specific octets, and the peer has to send an UPDATE at a moment of the test's choosing. The
reply comes from a **static event handler** registered on the client with
`set_client_event_handler_config`, which `try_execute_client_event_handler` serves before the
LLM budget is debited — so the answer is deterministic and no model is consulted for the event
under test.

| Test | Asserts | LLM calls |
|---|---|---|
| `update_handler_disconnect_reaches_the_peer` | after a handshake and one UPDATE, a handler answering `disconnect` produces a 21-octet NOTIFICATION checked field by field (marker, type 3, **code 6**, **subcode 2** — Cease / Administrative Shutdown), the socket then reaches EOF, and the client is `Disconnected` in `AppState` | 1, failed (`bgp_connected`) |
| `update_handler_wait_for_more_leaves_the_session_up` | the identical path with `wait_for_more` instead: nothing but KEEPALIVEs for 7s, no close, client still `Connected` | 1, failed (`bgp_connected`) |

The control is what makes the first test mean anything. Without it, a client that tore every
session down — or one whose NOTIFICATION came from a misfiring hold timer rather than from the
handler — would pass. The peer keeps keepaliving throughout the control so the 3s hold timer
cannot fire and supply a NOTIFICATION of its own.

**This is the test that fails without the fix.** `handle_update_message` passed `None` for the
write half, so the handler's actions were executed nowhere and logged as discarded; no
NOTIFICATION ever arrived and the first test times out at its 15s bound.

## Not covered

Multiple simultaneous peers, hold time 0 (no ticker is spawned; only the code path is
inspected), `send_notification` with a caller-chosen code reaching a peer (`disconnect` covers
the same write path), and interoperability with a real BGP daemon (none is installed).
