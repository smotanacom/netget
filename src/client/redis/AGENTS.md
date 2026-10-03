# Redis Client Implementation

## Overview

The Redis client implementation provides LLM-controlled access to Redis servers. The LLM can execute Redis commands and
interpret responses.

## Implementation Details

### Library Choice

- **Direct TCP connection**, no Redis client library. `resp.rs` frames RESP itself:
  `split_command` + `encode_command` on the way out, `read_reply` on the way in.
- **One complete reply per event.** `read_reply` reads a whole RESP2/RESP3 reply — a bulk
  string by its declared length, an array by its element count — so `GET k` answered with
  `$5\r\nhello\r\n` is one `redis_response_received`, not a `"$5"` event followed by a
  `"hello"` event. A line reader cannot do this: a bulk string's body may itself contain
  CRLF, so only its declared length says where it ends.

### Architecture

```
┌────────────────────────────────────────┐
│  RedisClient::connect_with_llm_actions │
│  - Connect to Redis via TCP            │
│  - Split stream (read/write)           │
│  - Spawn read loop                     │
└────────────────────────────────────────┘
         │
         ├─► Read Loop
         │   - resp::read_reply: one whole reply
         │   - Call LLM with response
         │   - Execute follow-up commands
         │
         └─► Write Half (Arc<Mutex<WriteHalf>>)
             - Send Redis commands
             - Format: RESP array of bulk strings
```

### LLM Control

**Async Actions** (user-triggered):

- `execute_redis_command` - Execute Redis command
    - Parameter: command (string)
    - Examples: "GET key", "SET key value", "HGETALL hash"
- `disconnect` - Close connection

**Sync Actions** (in response to Redis responses):

- `execute_redis_command` - Execute follow-up command based on response
- `wait_for_more` - Take no action and wait for more data (use when the reply is incomplete)

**Events:**

- `redis_connected` - Fired when connection established
- `redis_response_received` - Fired once per complete reply
    - `response`: redis-cli-style rendering (`OK`, `(integer) 1`, `"hello"`, `(nil)`,
      `(error) ERR …`; aggregates as JSON)
    - `reply_type`: `simple_string`, `error`, `integer`, `bulk_string`, `null`, `array`, `map`,
      `set`, `push`, `boolean`, `double`, `big_number`, `verbatim_string`
    - `value`: the reply as JSON — errors as `{"error": "…"}`, maps as objects, RESP3
      attributes read and dropped. Bulk strings that are not UTF-8 are converted lossily.

### Command Format

The model writes a command line the way it would type it at `redis-cli`; `split_command`
splits it with `redis-cli`'s own rules (`sdssplitargs`): whitespace separates arguments,
`"…"` allows `\n \r \t \b \a \\ \"` and `\xHH`, `'…'` allows `\'`, and a closing quote must be
followed by whitespace. So `SET greeting "hello world"` stores `hello world`; splitting on
whitespace would store `"hello` under `greeting` and send `world"` as a stray argument. An unbalanced quote is
**refused in `execute_action`** rather than guessed at. The arguments go out as a RESP array of
bulk strings:

```
*3\r\n$3\r\nSET\r\n$8\r\ngreeting\r\n$11\r\nhello world\r\n
```

### Bounds on what the server can make the client read

All in `resp.rs`, all checked against the length the server **declares**, before anything is
allocated for it: one reply may occupy at most `MAX_REPLY_BYTES` (16 MiB) including headers,
one aggregate may declare at most `MAX_AGGREGATE_LEN` (2^20) elements, a header line is at most
`MAX_LINE_LEN` (64 KiB), and nesting stops at `MAX_REPLY_DEPTH` (32). The reader is iterative,
but the `serde_json::Value` it builds is not: without the depth bound, ~40 KB of `*1\r\n` builds
a value whose drop overflows the stack and aborts the process — verified by removing the bound,
at which point `tests/client/redis/resp_reader_test.rs` aborts with `stack overflow`. A reply
that breaks a bound or is not RESP is a read error: the framing is gone, so the read loop sets
`ClientStatus::Error` and ends.

### Structured Actions

```json
// Command action
{
  "type": "execute_redis_command",
  "command": "SET user:123:name \"Ada Lovelace\""
}

// Response event
{
  "event_type": "redis_response_received",
  "data": {
    "response": "OK",
    "reply_type": "simple_string",
    "value": "OK"
  }
}
```

### Dashboard injection (`[ execute_redis_command ]`, `[ disconnect ]`)

`connect_with_llm_actions` registers a command channel
(`client::command_support::register_command_channel`) *before* the `redis_connected` LLM
call, which a manual rule can park. Because `read_reply` is not cancellation-safe, commands
are drained by a separate `command_loop` task (registered with `register_client_task`)
that shares the write half, not by a `select!` arm. `execute_redis_command` yields
`ClientActionResult::Custom { name: "redis_command" }`, which the generic
`handle_stream_client_command` cannot write, so `command_loop` routes the result through
`apply_action` — the one function the connected-event path and the read loop also use to
encode commands — then records an `injected_action` access-log entry and replies with
`ClientSendOutcome::Sent { bytes_sent }`. An injected `disconnect` half-closes; the read
loop sees EOF. The handle is removed (`remove_client_handle`) on every read-loop exit and
on the connect-time early return, so the rail stops offering `[ send ]` on a dead client.
Test: `tests/client/redis/command_channel_test.rs` (zero LLM calls).

### Dual Logging

```rust
info!("Redis client {} connected", client_id);           // → netget.log
status_tx.send("[CLIENT] Redis client connected");      // → TUI
```

## Limitations

- **Lossy non-UTF-8** - a binary bulk string reaches the model through
  `String::from_utf8_lossy`; there is no hex mode
- **No Connection Pooling** - Single connection per client
- **Pub/Sub untested** - after `SUBSCRIBE`, each pushed message is read as its own array (or
  RESP3 push) reply and raised as an event, but no test drives it
- **Pipelining is implicit** - several `execute_redis_command` actions in one answer are written
  back to back without waiting; each reply is then its own event, in order
- **No Authentication** - AUTH command can be sent manually
- **No Cluster Support** - Single server only

## Usage Examples

### GET Command

**User**: "Connect to Redis and get the value of user:123"

**LLM Action**:

```json
{
  "type": "execute_redis_command",
  "command": "GET user:123"
}
```

### SET Command

**User**: "Set the key 'status' to 'active'"

**LLM Action**:

```json
{
  "type": "execute_redis_command",
  "command": "SET status active"
}
```

### HGETALL Command

**User**: "Get all fields from hash user:123"

**LLM Action**:

```json
{
  "type": "execute_redis_command",
  "command": "HGETALL user:123"
}
```

## Testing Strategy

See `tests/client/redis/CLAUDE.md` for E2E testing approach.

## Future Enhancements

- **Authentication** - Built-in AUTH handling
- **Cluster Support** - Redis Cluster client
- **Connection Pooling** - Multiple connections

## Maturity: Beta

Rated against the four-condition client bar in the root `CLAUDE.md`, on the evidence in
`tests/client/redis/real_server_test.rs` (see `tests/client/redis/CLAUDE.md`):

1. **Real third-party server** — a real `redis-server` (Valkey locally, Redis on Ubuntu), read back with `redis-cli`; NetGet's side is no Redis client library at all (`resp.rs` is ours), so no code is shared.
2. **Fails rather than skips** — a missing `redis-server` or `redis-cli` is a test failure naming the brew formula and the
   Ubuntu package (`tests/helpers/real_server.rs`); nothing is `#[ignore]`d. CI's
   `registry-audit` installs the peer and runs the suite in its evidence loop.
3. **A real session** — the protocol's own exchange, with the server's answers parsed and handed
   to the model, not a connect.
4. **Acts on the model's answer, asserted on the wire** — `redis-cli` reads back values the model wrote, one of them built from a reply it was shown. Verified by mutation: dropping
   the actions the model returned makes the test fail.

Not covered by that evidence: AUTH/SELECT as parameters, Pub/Sub and RESP3 against a real server.
