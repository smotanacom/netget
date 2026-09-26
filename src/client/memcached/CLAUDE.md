# Memcached client

NetGet dials a memcached server and the model drives the **text protocol**: which keys to read,
what to store, when to delete, increment or touch. The binary protocol is not implemented; it
was deprecated upstream in 1.6.

## Files

- `wire.rs` — the grammar. `Request` (validated, encodes itself, never `noreply`), `Expect` (what
  the reply to a request looks like) and `parse_reply`, which reads one **whole** response off a
  buffer or says it needs more bytes. Pure functions, no I/O.
- `actions.rs` — the vocabulary the model sees, the fourteen event types, and
  `request_from_action`, where every refusal happens before anything reaches the wire.
- `mod.rs` — the connection: three tasks, all registered with `spawn_client_task`.

## The three tasks

| task | owns | never waits on |
|---|---|---|
| transport | the socket, the FIFO of requests in flight, the reply buffer | the model |
| turns | the model calls, one queued reply at a time, in order | the socket |
| commands | injected actions (`[ send ]`, MCP `send_to_client`) | the model |

A memcached reply does not say what it answers — `STORED`, `NOT_FOUND`, a bare number and `END`
are only meaningful against the request they follow — so the transport keeps a FIFO of `Expect`s
and pairs each complete reply with the oldest one. Requests reach the transport over one channel
from both the turn task and the command task, so an injected action and a model action take the
same path onto the wire (`apply_action`) and the FIFO order is the wire order.

The chain *request → reply → model → request* passes through the turn queue, so it needs no
recursion and no `MAX_FOLLOWUP_DEPTH`: each reply is a new queued turn. A model that keeps
issuing requests keeps the conversation going, as it does in `redis`.

## Events

One reply can become several events: a `get`/`gets` becomes **one event per requested key**, in
request order — `memcached_value {key, value, flags, cas?}` for a hit, `memcached_miss {key}` for
a miss. Status replies become `memcached_stored` / `not_stored` / `exists` / `not_found`
(`{command, key}`), `memcached_deleted`, `memcached_touched`, `memcached_counter {command, key,
value}`, `memcached_version`, `memcached_ok` (flush_all), `memcached_stats {stats: {name: value},
group?}`. `ERROR` / `CLIENT_ERROR` / `SERVER_ERROR` become `memcached_error {kind, message,
command, key?}`; a status line that is not a reply to the request in flight becomes
`memcached_error {kind: "unexpected_reply"}`.

`memcached_connected` is the connect event, so the dashboard's zero-action connect rule applies.

## Values are text

A value the model stores is the UTF-8 bytes of the string it wrote, with its length as the byte
count, so spaces and newlines in a value are data. A value the server returns that is **not**
valid UTF-8 is refused: `memcached_error {kind: "non_text_value", key, message}` names the key and
its length. It is never re-encoded (hex, base64, lossy): the model would write that encoding back
as a different value.

## Refusals (before the wire)

- keys: 1..=250 bytes, no space or control octet — a key is one token on a CRLF-terminated line,
  so either would turn the model's key into a different command;
- `get`/`gets`: 1..=32 keys, no key twice (each key is a model turn);
- `flags`: must fit 32 bits (`u32::try_from`, never `as u32`);
- values over memcached's 1 MiB default item limit;
- `stats` group: one lowercase word;
- **`memcached_flush_all` unless `"confirm": true`** — it invalidates every item for every client.

## Bounds on what the server can make the client hold

All in `wire.rs` / `mod.rs`, each with a test that fails when it is removed:

| bound | value | where |
|---|---|---|
| reply line without CRLF | 2 KiB (`MAX_REPLY_LINE`) | `line_at` |
| declared `VALUE` size | 1 MiB, checked on the **declared** number before the block is waited for | `parse_reply` |
| one response | 4 MiB (`MAX_RESPONSE_BYTES`) — 32 legal 1 MiB values are not a legal response | `parse_reply` |
| `STAT` lines per response | 4096 | `parse_reply` |
| `VALUE` keys | only keys that were requested, each once | `parse_reply` |
| requests in flight | 128 (`MAX_IN_FLIGHT`) — the next is refused | transport |
| replies waiting for the model | 256; a reply past that is dropped with `decision=turn_queue_full` | `drain_replies` |

The grammar has no nesting, so there is no depth to bound. Bytes arriving with no request in
flight fail the connection: framing is unknown from then on.

## Logging

Every model outcome logs `decision=model_actions`, `decision=model_silent` or
`decision=llm_error` with the event id. Server-supplied text in events (error messages, the
version string) goes through `utils::sanitize::line_field`; values do not, because they are the
model's data.

## Limitations

No SASL, no meta commands (`mg`/`ms`/`md`), no `verbosity`/`cache_memlimit`, no per-request
timeout (memcached always answers; a server that does not leaves the client waiting), one server
per client (no consistent hashing across a pool).

## Maturity: Beta

All four conditions of the client bar hold, on `tests/client/memcached/real_server_test.rs`:

1. the peer is the real C memcached, prepared and read back with libmemcached's `memcp`/`memcat`
   — nothing NetGet wrote, and not a library this client is built on (it frames the protocol
   itself);
2. a missing `memcached`, `memcp` or `memcat` fails the test, never skips it;
3. a real session: set, gets across four keys, a second set, all answered by the server;
4. the client acts on the model's answer, asserted from the server's side by `memcat` — and
   emptying the loop over `result.actions` fails it.

Passed three consecutive runs at `--test-threads=100` before promotion.
