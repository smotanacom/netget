# Redis Protocol E2E Tests

Four files, all declared in `tests/server/redis/mod.rs`.

| File | Tests | What it proves | LLM calls |
|---|---|---|---|
| `e2e_test.rs` | 6 | Every RESP2 reply type, through `redis-rs` | 13 |
| `resp_framing_test.rs` | 3 | Model output cannot split a frame; `stop_server` stops sessions | 0 |
| `llm_failure_test.rs` | 1 | The RESP error a client sees when the backend fails | 1 |
| `peer_inject_test.rs` | 1 | Dashboard injection reaches the socket | 0 |

## Running

`--test` names a **target**, not a module path. `--test server::redis::e2e_test`
makes cargo list its targets and exit having run nothing — and it exits 0, so it
looks like a pass. Filter after `--`:

```bash
./cargo-isolated.sh test --no-default-features --features redis \
    --test server -- server::redis --test-threads=100
```

## What the Beta rating rests on

`e2e_test.rs` drives **redis-rs** (`redis` 0.27, a dev-dependency), an
independent implementation of RESP2 — not the `redis-protocol` crate the server
parses with, so the evidence is not circular. Six tests, module
`redis_server_tests`:

| Test | Reply type |
|---|---|
| `test_redis_ping_with_mocks` | simple string `+PONG\r\n` |
| `test_redis_get_set_with_mocks` | simple string + bulk string |
| `test_redis_integer_response_with_mocks` | `:42\r\n` |
| `test_redis_array_response_with_mocks` | `*3\r\n…` |
| `test_redis_null_response_with_mocks` | `$-1\r\n` |
| `test_redis_error_response_with_mocks` | `-ERR …\r\n` |

None is `#[ignore]`d and none skips when something is missing — redis-rs is
compiled in, so there is no binary to be absent. `llm_failure_test.rs` also reads
its assertion back through redis-rs.

## LLM call budget

`e2e_test.rs`: 6 servers × 1 startup + 7 command calls = **13**. That is over the
~10 guideline, and the honest reason is that each test covers a distinct RESP2
encoding and consolidating them would make a failure harder to localise. The
other three files cost **1** between them.

`resp_framing_test.rs` and `peer_inject_test.rs` build their servers directly
through `ServerForm` with a `*` static handler and point at an unreachable
backend, so an accidental model call fails the test rather than passing quietly.

Both pass **`instruction: Some(String::new())`**, which is load-bearing:
`ServerForm::create` substitutes a default instruction whenever `instruction` is
`None`, and any non-empty instruction makes `operator_wants_dynamic` true — so a
server built with `..Default::default()` consults the model whatever the test's
comments claim.

## Binary safety — the case worth knowing

`resp_framing_test.rs` exists because RESP has two kinds of string and only one
of them is safe for generated text:

- A **bulk** string is length-prefixed, so it can carry any bytes.
- A **simple** string (`+…`) and a **simple error** (`-…`) are CRLF-terminated
  with no length. A newline inside the payload ends the frame early, and
  everything after it is parsed as the next reply.

`redis_simple_string`'s `value` and `redis_error`'s `message` are model output.
Before this was guarded, one newline in a model's error prose desynchronised the
connection permanently — every later command read the previous one's leftovers,
and the client could not tell. The server now maps CR and LF to spaces, as Redis
itself does.

The decisive assertion is not that the first reply is well-formed; it is that the
**second** command gets its own reply rather than the tail of the first.

## Scripting

The `e2e_test.rs` suite uses `ServerConfig::new()`, which disables scripting, so
each reply comes from an action and the action-to-wire encoding is what is under
test. A script handler would prove the script ran, not that the encoding is
right.

## Known limitations of the suite

- **`redis-cli` is not driven anywhere.** redis-rs is a genuine third-party
  implementation and is what the rating rests on; a skip-when-missing gate around
  a binary would not be evidence anyway.
- **RESP3 is not tested** because the server does not implement it (no `HELLO 3`).
- **Inline commands** (`PING\r\n` typed into `nc`) are not tested; only RESP
  arrays decode, and both redis-cli and redis-rs always send arrays.
- **Pipelining** — the read loop processes several frames from one read in order,
  each with its own LLM call, but no test sends two commands in one write.
- **AUTH / SELECT / MULTI / pub-sub** reach the model as ordinary commands with no
  special handling, and nothing asserts on them.

## Fixed, recorded so the notes are not re-added

- *"No Response Fallback: if the LLM returns no action, client hangs
  indefinitely."* It does not: the server replies
  `-ERR no response produced for this command` and logs
  `decision=fail_closed_no_action`.
- *"redis v0.25"* — the dev-dependency is 0.27.
- Test names here omitted the `_with_mocks` suffix every one of them carries.
