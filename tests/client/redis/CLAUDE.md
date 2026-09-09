# Redis Client E2E Tests

Two files, declared in `tests/client/redis/mod.rs`.

| File | Tests | LLM calls |
|---|---|---|
| `e2e_test.rs` | 2 running (`*_with_mocks`), 2 `#[ignore]`d | 6 |
| `command_channel_test.rs` | 1 | 0 |

## Running

`--test` names a **target**, not a module path — `--test client::redis::e2e_test` makes cargo
list its targets and exit having run nothing, and it exits 0, so it reads as a pass:

```bash
./cargo-isolated.sh test --no-default-features --features redis \
    --test client -- client::redis --test-threads=100
```

## Tests

### `test_redis_client_connect_and_command_with_mocks` (3 calls)

Startup, `redis_connected` → `execute_redis_command PING`, `redis_response_received` →
`set_memory`. The peer is a NetGet Redis **server** with its own mock, so both ends are under
test.

### `test_redis_client_llm_controlled_commands_with_mocks` (3 calls)

The same shape with a GET/SET instruction.

### `command_channel_test::injected_redis_command_reaches_our_own_server` (0 calls)

`AppState::send_to_client` puts a command on the wire without the model — the dashboard's
`[ send ]` path.

### The two `#[ignore]`d tests

`test_redis_client_connect_and_command` and `test_redis_client_llm_controlled_commands` are the
un-mocked originals, kept for `--use-ollama` runs. They configure no `.with_mock()`, so under
the default strict-mock mode the LLM call 500s immediately and the client never connects.

## No Docker

An earlier version of this file described the suite as "unit tests for Redis client state
management" with "0 calls (unit tests only)", listed two tests
(`test_redis_client_initialization`, `test_redis_client_status`) that **do not exist in the
file**, and said full integration would need `docker run -p 6379:6379 redis:latest`.

None of that is how this works. The peer is a NetGet Redis server started by the same harness,
so there is no external dependency and no container — and the tests that exist drive real RESP
over a real socket, not field assertions on a struct.

## What the mocks must get right

Answer `redis_response_received` with `set_memory` or `wait_for_more`, not with something the
client cannot execute. Both are handled; a name the executor does not know is logged and
nothing goes on the wire, which is easy to mistake for success.

`disconnect` is worth knowing about: until recently, asking for it here did **nothing**. The
read loop's `break` left only the `for action in actions` loop, so the socket stayed open, the
command handle was never dropped and the status never changed — while the connect-time path
did all three. Nothing asserted on it, which is why it survived.

## Not covered

- **AUTH / SELECT.** The client declares no startup parameters at all and reads none, so a
  database index or password can only be reached by an explicit `execute_redis_command`.
- **Multi-line replies.** The read loop is `read_line`, so one RESP *line* is one LLM call; a
  bulk reply spanning lines costs several turns, and `wait_for_more` is how the model says
  "that was partial". No test exercises it.
- **A hostile or hallucinating server.** `read_line` has no cap and the connect has no timeout.
