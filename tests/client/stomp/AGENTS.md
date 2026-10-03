# STOMP Client E2E Test Documentation

`tests/client/stomp/e2e_test.rs` — **8 tests, 8 LLM calls**, all in one test; the other seven
make zero.

```bash
./cargo-isolated.sh test --no-default-features --features stomp \
    --test client::stomp::e2e_test -- --test-threads=100
```

## Strategy: two peers, for two different jobs

| Test | Peer | LLM calls |
|---|---|---|
| `test_stomp_client_session_against_the_netget_broker` | NetGet's own STOMP server, separate process, real socket | 5 server + 3 client |
| `test_connect_frame_is_well_formed_and_a_valid_connected_opens_the_session` | hand-written broker in the test | 0 |
| `test_connected_without_a_version_header_is_refused` | hand-written broker | 0 |
| `test_connected_naming_a_version_we_did_not_offer_is_refused` | hand-written broker | 0 |
| `test_a_non_connected_reply_is_refused_and_says_what_arrived` | hand-written broker (×2) | 0 |
| `test_a_silent_broker_times_out_rather_than_hanging` | a listener that never answers | 0 |
| `test_actions_produce_the_frames_the_specification_defines` | none — the executor is pure | 0 |
| `test_invalid_action_parameters_are_refused_with_a_reason` | none | 0 |

**The session test is the interoperability evidence** and the handshake tests are the
*strictness* evidence, and neither substitutes for the other. A correct server never sends a
`CONNECTED` without a `version` header, so the only way to prove this client refuses one is a
peer that gets it wrong on purpose — which only a socket the test owns can be. Conversely a
hand-written peer proves nothing about whether the two implementations actually interoperate.

## What the session test asserts, and how

Nothing in it inspects bytes. Every step is pinned by a mock expectation on one side or the
other, which is stronger: an expectation of exactly one call fails both when the step did not
happen and when it happened twice.

| Mock rule | What its `expect_calls(1)` proves |
|---|---|
| server `stomp_connect` | the client's `CONNECT` was well-formed enough for the server to raise the event |
| server `stomp_subscribe` | the client subscribed after `stomp_connected` — and the rule quotes the client's own `id` back in the `MESSAGE`, so a delivery naming any other subscription would be visibly wrong |
| client `stomp_message_received` | the `MESSAGE` was parsed and reported to the model |
| server `stomp_send` | the client published in response to a delivery — the action → wire → event → action chain closed |
| server `stomp_disconnect` | the client left with a `DISCONNECT` frame rather than dropping the socket |

The client then waits for `"disconnected"` in its own output, which only happens once the read
loop has exited — and given that the server received the `DISCONNECT` and answers it with an
automatic `RECEIPT`, that is the graceful path completing rather than `DISCONNECT_GRACE`
lapsing.

### The mock rules are first-match-wins, and the startup rule is the trap

The startup rule matches on `instruction contains "open a stomp client to"` — text that appears
only in the top-level prompt, never in the `open_client` instruction the rule itself produces
(`"Subscribe, echo each delivery, then leave"`). If those two strings overlapped, the startup
rule would swallow every later event call and the event rules would all report zero. Keep them
textually disjoint.

The event rules use one rule per event, and the two that need to vary use
`respond_with_actions_from_event` rather than a second indistinguishable rule.

## Why `async-stomp` is not the peer here

It is a *client*. It is the Beta evidence for `src/server/stomp/`, where it is the opposite
number; it cannot be the opposite number of another client. There is no STOMP broker crate in
this tree and none on macOS or in Homebrew.

`src/client/stomp/CLAUDE.md` sets out, step by step, the ActiveMQ / RabbitMQ test that would
promote this client to Beta — including the two assertions that a lenient broker would let a
buggy client pass without: that the delivery was *acknowledged* rather than redelivered (which
is what catches an `ACK` quoting `message_id` instead of the `ack` header), and that the session
closed on the `RECEIPT` rather than on the grace timeout.

**Do not add a skip-when-missing gate for that test.** `npm`'s real-CLI test is the shape to
copy: it fails when the binary is absent, because a silent skip would leave the maturity rating
resting on nothing.

## Runtime

- Handshake and executor tests: milliseconds each. `test_a_silent_broker_times_out_rather_than_hanging`
  sets `handshake_timeout_secs: 1` through a real `StartupParams` and asserts the elapsed time,
  so it costs a second and would fail loudly if the parameter stopped being read.
- The session test: ~3s, dominated by process startup on both sides.
- The whole file at `--test-threads=100`: ~3s.

## Things that will bite whoever extends this

- **`start_netget_client` requires exactly one client and zero servers** from the prompt, so the
  server has to be started by a separate `start_netget_server` call with its own mock.
- **The binary is shared.** `target/debug/netget` is rebuilt by every other agent's narrow
  feature build, and both e2e tests here spawn it. A failure that says the protocol is not
  compiled in, or a timeout with no output, is a contention artefact — check for other `cargo
  test` processes before believing it. Four `server::stomp` tests failed this way during this
  client's development and passed on every re-run.
- **`broker_answering` holds its socket open for three seconds after replying.** That is
  deliberate: without it a failure could be about the connection vanishing rather than about
  the frame the client read, and the two look identical from the error message.
- **The handshake tests use an unreachable Ollama URL (`127.0.0.1:1`).** None of them registers
  a client in `AppState`, so `get_instruction_for_client` returns `None` and no model call is
  ever attempted. A reachable URL there would only hide a mistake.
