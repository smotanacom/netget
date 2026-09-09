# Socket File Client Testing

## Strategy

The peer of a Unix-domain-socket client is a **local descriptor**. There is no third-party
implementation of "a program listening on a Unix socket" to validate against, the way `mysql`
has `mysql_async` — so the strongest evidence this protocol admits is a **real
`tokio::net::UnixListener` peer**, bound in the test process, accepted for real, with real bytes
crossing in both directions. That is what the suite drives. It is why the client stays
`Experimental` and why no test here should be read as evidence for more.

Everything runs in-process against a `MockOllamaServer`; no NetGet binary is spawned, so the
suite is immune to the shared-`target/` contention that makes binary-spawning tests flaky during
a parallel wave.

## The tests

### `e2e_test.rs::client_speaks_first_and_answers_the_peer` — 2 LLM calls

The only test that exercises the read loop.

1. A `UnixListener` is bound at a per-process, per-nanosecond temp path and accepts one
   connection, forwarding every chunk it reads to the test and answering the first with `PONG\n`.
2. `ClientForm::create` starts the client. The connect handling raises
   `socket_file_connected`; the mock answers `send_socket_file_data` `PING\n`; the test asserts
   `PING\n` arrived at the peer. **A connect event that is never raised fails here** — that
   event was declared and unemitted for a long time, which left a client that was told to speak
   first unable to.
3. The peer's `PONG\n` raises `socket_file_data_received`. The mock rule matches on
   `and_event_data_contains("data", "PONG")` — **in plain text**, which is the point: before this
   pass the event carried `data_hex` only, and this rule would have had to say `504f4e470a`.
4. The mock answers `ACK\n` and the test asserts it reached the peer, which proves the model's
   answer is executed rather than counted and logged.

Both rules are `expect_calls(1)` and the test finishes with `verify_calls()`, so a rule that
never matched fails rather than falling through to a real model.

Waits are `tokio::time::timeout` on the forwarding channel — a condition, never a sleep.

### `e2e_test.rs::send_accepts_text_hex_and_the_legacy_field` — 0 LLM calls

The payload contract, at the executor. `{"data": "Hello"}` sends `Hello`;
`{"data": "48656c6c6f"}` sends the ten characters, **not** `Hello`, because there is no
auto-detection; `{"data": "48656c6c6f", "encoding": "hex"}` sends the five bytes; the legacy
`{"data_hex": ...}` still works. Invalid hex is an `Err` naming the encoding, not a panic and not
a silent truncation.

### `e2e_test.rs::declared_events_match_the_emitted_ones` — 0 LLM calls

`get_event_types()` must return the statics the read loop emits. It used to return two freshly
built `EventType`s with `{"type": "placeholder"}` examples, no parameters and no attached
actions. The test asserts the real parameters are declared, that `data_hex` is *not* the
model-facing field, that each event attaches `send_socket_file_data`, and that neither example is
a placeholder.

### `e2e_test.rs::metadata_is_experimental_and_names_its_evidence` — 0 LLM calls

Pins the maturity rating with the reason in the assertion message.

### `command_channel_test.rs::injected_socket_file_data_reaches_the_unix_socket` — 0 LLM calls

The dashboard's `[ send ]` path: `AppState::send_to_client` injects an action from outside the
read loop and the bytes reach the socket. The client's LLM points at `http://127.0.0.1:1`, so
nothing here goes through a model. Also asserts an unknown action is `Rejected` rather than
swallowed, and that an injected `disconnect` really ends the loop and drops the handle.

## LLM call budget

**2 calls**, both mocked, both in one test.

## Expected runtime

Under a second for the whole directory; the last measured run was 5 tests in 0.17s.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features socket_file \
    --test client -- socket_file:: --test-threads=100
```

Note `--test client` names the *target*; `--test client::socket_file::e2e_test` lists targets and
exits without running anything.

## Not covered

- `wait_for_more` accumulation: the action is advertised and the executor returns `WaitForMore`,
  but the read loop ignores that variant — it processes the queue on the next pass regardless.
- Payloads larger than the 8 KiB read buffer, and binary payloads over the wire (the `hex`
  encoding is covered at the executor only).
- Real services (`/var/run/docker.sock`, a PostgreSQL socket). Those are manual checks; a test
  that skips when the socket is absent would be a silent pass, which CLAUDE.md is explicit is not
  evidence.
- Client `event_handlers` dispatch (script/static/manual) on this protocol.

## Known issues

**Socket file cleanup.** Paths include the PID and a nanosecond timestamp, and are removed before
bind and after the assertions, so a crashed run cannot collide with a later one. `/tmp` is used
rather than `./tmp` so a test never depends on the repo cwd.

**Platform.** Unix only; the whole suite is `#![cfg(all(feature = "socket_file", unix))]`.
