# Redis Client E2E Tests

Four files, declared in `tests/client/redis/mod.rs`. Nothing is `#[ignore]`d.

| File | Peer | Tests | LLM calls |
|---|---|---|---|
| `real_server_test.rs` | **real `redis-server`** + `redis-cli` | 2 | 9 |
| `resp_reader_test.rs` | none — bytes fed to `resp::read_reply` / `split_command` | 7 | 0 |
| `e2e_test.rs` | NetGet's own Redis server | 2 | 6 |
| `command_channel_test.rs` | NetGet's own Redis server | 1 | 0 |

## Running

`--test` names a **target**, not a module path — `--test client::redis::e2e_test` makes cargo
list its targets and exit having run nothing, and it exits 0, so it reads as a pass:

```bash
./cargo-isolated.sh test --no-default-features --features redis \
    --test client -- client::redis --test-threads=100
```

## `real_server_test.rs` — the evidence the rating rests on

The peer is the real C server (`redis-server`; Valkey on Homebrew, Redis on Ubuntu), spawned per
test by `tests/helpers/real_server.rs` on a probed loopback port with `--save "" --appendonly no`,
ready when it logs `Ready to accept connections`. State is read back with `redis-cli`. **It fails,
never skips,** when either binary is missing, naming `brew install valkey` /
`apt-get install redis-server redis-tools`.

### `redis_client_writes_reads_and_acts_on_a_reply_against_redis_server` (5 calls)

`redis_connected` → `SET netget:greeting "hello from the model"` (a quoted value with a space);
the `+OK` event (matched on `reply_type` `simple_string`) → `GET netget:greeting`; the bulk
reply event (matched on `reply_type` `bulk_string` **and** `value` containing the whole string)
→ `RPUSH netget:log "the model saw: <value>"`; the `:1` event (matched on `integer` and
`(integer) 1`) → nothing. Then `redis-cli GET` must print `hello from the model` and
`redis-cli LRANGE` must print `the model saw: hello from the model`.

### `redis_client_reads_an_array_reply_prepared_by_redis_cli` (4 calls)

`redis-cli HSET` writes a hash whose values contain a space and a real newline. The model sends
`HGETALL`; the reply reaches it as **one** `array` event, and it stores a summary computed from
the parsed array; `redis-cli GET` must print `4 elements; field2 has 9 chars`. A line-framed
reader would have split the newline-bearing value and got the count wrong.

### Why this is condition 4 of the client bar

The assertions are on the server's state, written by commands the model chose — one of them
built from a reply the model was shown. Verified by mutation: dropping the actions in the read
loop's `for action in actions` makes both tests fail while the mock still records its calls.
And the per-reply matchers are the regression test for the framing: under the old line reader
the bulk reply arrived as `"$20"` then `"hello from the model"`, neither of which matches.

No sleeps: the last mock rule in each test is the reply to the model's last command, so once
`wait_for_mocks` returns, the server has applied everything the model sent.

## `resp_reader_test.rs` — framing and bounds (0 calls)

A bulk string is one reply, CRLF inside it included; every RESP2 type and a pipeline of seven
replies; RESP3 map/set/push/boolean/double/big number/verbatim, and an attribute that is dropped
without counting as an element; `redis-cli`-compatible command splitting. And the bounds, each
verified by removing it:

- **depth** (`MAX_REPLY_DEPTH` 32): 10 000 × `*1\r\n` is refused. Without the bound the whole
  test binary aborts with `fatal runtime error: stack overflow` — the value is built iteratively
  but dropped recursively.
- **declared string length**: `$4294967295` and `$16777216` are refused before any allocation.
- **declared aggregate length**: `*1048577` is refused.

## `e2e_test.rs` and `command_channel_test.rs` — same-project

The peer is NetGet's own Redis server, so these show the two halves agree — circular evidence,
kept for what it does cover: `test_redis_client_connect_and_command_with_mocks` (3 calls) and
`test_redis_client_llm_controlled_commands_with_mocks` (3 calls) drive the client through a
server whose own mock is asserted too, and `command_channel_test` (0 calls) puts a command on the
wire through `AppState::send_to_client`, the dashboard's `[ send ]` path.

## What the mocks must get right

Answer `redis_response_received` with `execute_redis_command`, `wait_for_more`, `disconnect` or
nothing. A name the executor does not know is logged and nothing goes on the wire, which is easy
to mistake for success. Match on `reply_type` / `value`, not on raw RESP: the event carries the
parsed reply, never the bytes.

## Not covered

- **AUTH / SELECT.** The client declares no startup parameters, so a password or database index
  can only be sent as an explicit `execute_redis_command`.
- **Pub/Sub** against a real server, and **RESP3** (`HELLO 3`) against a real server — both are
  parsed (see `resp_reader_test.rs`) but not exercised end to end.
- **Connect timeout.** A server that accepts and never speaks leaves the client waiting.
