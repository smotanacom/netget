# NATS E2E Testing

`tests/server/nats/e2e_test.rs`. Seven tests, **8 LLM calls total**.

## What each test is for

| Test | LLM calls | Proves |
|---|---|---|
| `test_nats_delivers_model_authored_message_to_async_nats` | 4 | the real client accepts everything this server writes |
| `test_nats_greeting_keepalive_and_protocol_error` | 2 | `INFO` shape; `PING`/verbose `+OK` cost **no** LLM call; a bad frame gets `-ERR` and a hang-up |
| `test_nats_llm_failure_answers_with_a_category_only` | 2 | the fail-closed `-ERR` is a category, with no internals |
| `test_nats_parser_frames_control_lines_and_payloads` | 0 | framing, including partial frames |
| `test_nats_parser_reads_hpub_headers_and_binary_payloads` | 0 | `HPUB` header block + binary payload |
| `test_nats_parser_rejects_bad_frames` | 0 | the four `FrameError`s |
| `test_nats_subject_matching` | 0 | `*` and `>` wildcards |

## The maturity evidence

**`test_nats_delivers_model_authored_message_to_async_nats` is what the `Beta`
rating rests on.** `async-nats` 0.50 — the official NATS Rust client, and no part
of this server's implementation — completes a real handshake, subscribes,
publishes, and then receives two messages the model authored. Everything is
asserted from inside the client's own `Message`:

- the payload quotes what the client published (`"the model saw: hi"`), so the
  event data reached the model and its answer came back;
- the second message is an `HMSG`, and its `reply` subject and `Nats-Msg-Id`
  header are read back out of the client's parsed `HeaderMap`.

Two properties the root CLAUDE.md asks for:

- **Not `#[ignore]`d.** It runs in every normal invocation.
- **Cannot skip.** `async-nats` is a dev-dependency, so it exists wherever the
  suite compiles. There is no `SKIP: … is not installed` path to hide behind —
  that pattern is why `kubernetes`, `oci_registry`, `maven` and `websocket` were
  *not* promoted.

It is also not the circular case (`webrtc_signaling`/`websocket` driven by the
same crate their server frames with): this server uses no NATS library at all.

## The sid must come from the event

The subscription id is chosen by the **client** (`async-nats` counts from 1, but
that is its business, not the protocol's). The `nats_publish` rule therefore uses
`respond_with_actions_from_event` and reads
`e["matching_subscriptions"][0]["sid"]`, exactly as the UDP protocols echo a
transaction id. A hardcoded sid would be silently dropped by the client — a `MSG`
for an unknown sid is ignored, not rejected — and the test would fail two steps
later on a missing message.

## Mock expectations

Rules are first-match-wins, and each rule here matches a **distinct event id**,
so there is no ambiguity to get wrong. The startup rule is
`on_instruction_containing("listen on port") + and_instruction_containing("nats")`;
the rest are `on_event(...)`, narrowed with `and_event_data_contains` where more
than one instance of the event could occur.

`nats_connect` and `nats_subscribe` are answered with an **empty** action array.
That is a real answer ("say nothing"), it keeps the raw-socket test's byte stream
deterministic, and it is the established pattern (the BLE suites do the same).

Every test ends with `wait_for_mocks(30)` then `verify_mocks()`.

### Why the keepalive test is an assertion about counts

`test_nats_greeting_keepalive_and_protocol_error` sends `CONNECT` + `PING` and
asserts `+OK` and `PONG` come back — but the thing that proves they were written
in Rust is `verify_mocks()`: the only event rule is `nats_connect` with
`expect_calls(1)`. If `PING` or the verbose acknowledgement went through the
model, either that rule would over-count or an unmatched request would fall
through to the mock's HTTP 500 and the connection would be torn down mid-test.

### The fail-closed test deliberately has a hole in its rules

`test_nats_llm_failure_answers_with_a_category_only` registers rules for startup
and `nats_connect` and **nothing** for `nats_publish`, with no `on_any()`
catch-all. The mock answers that request with HTTP 500, netget sees a genuine
backend failure, and the peer must get `-ERR '<WireFailure category>'`. The test
then scans the reply for `http://`, `127.0.0.1:`, `ollama`, `llama`, `src/`,
`.rs`, `retries` and `LLM`.

An unmatched request is recorded only as a harness diagnostic, so `verify_mocks()`
still passes — a catch-all rule here would answer the publish and the test would
prove nothing.

## Runtime

~1 second for the whole file (three netget subprocesses, all mocked). The parser
tests are microseconds.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features nats \
    --test server -- --test-threads=100 nats
```

Note `--test server`, not `--test server::nats::e2e_test`: the latter is not a
cargo target and cargo answers by listing every test binary in the repo.

## A failure that is not yours

`Protocol 'NATS' exists but is not compiled into this build` from all three
subprocess tests means `target/debug/netget` was replaced mid-run by another
agent's build with a different feature set. `target/` is shared. Re-run before
believing it; the same three tests passed immediately before and after when this
happened here.

## Not covered

Multiple concurrent connections, queue-group behaviour, `UNSUB` with a max,
`send_ping`, `send_nats_info` mid-connection, payloads at the `max_payload`
boundary, and anything requiring a second client — none of which the server
implements beyond what is described in `src/server/nats/CLAUDE.md`.
