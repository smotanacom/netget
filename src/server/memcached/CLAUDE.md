# Memcached Server (text protocol)

TCP 11211. The model is the cache.

Files: `protocol.rs` (pure parsing and reply framing), `actions.rs` (LLM vocabulary +
executor), `mod.rs` (listener, per-connection loop).

## The rule this protocol exists to demonstrate: it stores nothing

There is **no map, no table, no file, no persistence of any kind** in this server. Grep for
`HashMap` in `src/server/memcached/` and you will find none. Every `get` is a question put to
the model; every `set` asks the model whether it would have stored the item.

That is the project rule — protocols must not implement storage — and here it is also the
whole point. A hash map is not worth emulating. A model deciding what `session:42` holds is.

The visible consequence, documented in `metadata().notes` so nobody is surprised: **two
successive `get`s of the same key are two independent questions and may legitimately return
different values.** If a key must keep its value, use a script handler (deterministic, no LLM
call) or the generic SQLite facility in `src/state/sqlite.rs` that the model opts into at
runtime. Neither belongs in this directory.

## Wire format

Text protocol only. The binary protocol was deprecated in memcached 1.6 (2020) and is no
longer documented upstream; the meta commands (`mg`/`ms`/`md`) are the sensible next
addition, not binary.

Implemented commands: `get`, `gets`, `set`, `add`, `replace`, `append`, `prepend`, `cas`,
`delete`, `incr`, `decr`, `touch`, `stats`, `version`, `flush_all`, `quit`.

**Byte counting is the thing to get right.** A storage command is
`<cmd> <key> <flags> <exptime> <bytes> [<cas>] [noreply]\r\n` followed by *exactly* `<bytes>`
octets and then `\r\n`. The data block may itself contain `\r\n`, so `parse_storage` counts
rather than scans — searching for a delimiter is the classic memcached implementation bug and
it corrupts every subsequent command on the connection.

Retrieval framing:

```text
VALUE <key> <flags> <bytes>[ <cas unique>]\r\n
<data block>\r\n
...
END\r\n
```

`<bytes>` is computed by `encode_values` from the actual payload length and is **never taken
from the model**. A count that disagrees with the payload desynchronises the client parser
for the rest of the connection, and a model asked to count bytes will eventually miscount.
An empty value list is a cache miss: `END\r\n` alone.

`noreply` is honoured on storage, delete, arithmetic, touch and flush_all: the LLM call still
happens, only the write is skipped.

`quit` is handled in `mod.rs` without an LLM call — it has no reply, so there is nothing to
decide.

## Events and actions

Nine events, one per command shape. Every one is raised by `mod.rs::event_for`, and every one
carries `.with_actions(...)`:

| Event | Raised by | Typical answer |
|---|---|---|
| `memcached_get` | `get` / `gets` | `send_memcached_values` |
| `memcached_store` | `set`/`add`/`replace`/`append`/`prepend`/`cas` | `send_memcached_status` |
| `memcached_delete` | `delete` | `DELETED` / `NOT_FOUND` |
| `memcached_arithmetic` | `incr` / `decr` | `send_memcached_number` |
| `memcached_touch` | `touch` | `TOUCHED` / `NOT_FOUND` |
| `memcached_stats` | `stats` | `send_memcached_stats` |
| `memcached_version` | `version` | `send_memcached_version` |
| `memcached_flush_all` | `flush_all` | `OK` |
| `memcached_unknown_command` | any other verb | `send_memcached_error` |

`close_memcached_connection` is attached to **every** event as well. It is in
`get_sync_actions()`, but a server's tool list is built from `event.event_type.actions` and
the `get_sync_actions()` fallback only fires when the event declares none — so while it was
listed on no event, the model could never close a connection however it was prompted, and the
executor arm was reachable only through dashboard injection.

Values carry an explicit `value_encoding` of `"utf8"` (default) or `"hex"`, in both
directions, and the executor really decodes hex (`decode_value`). This follows the
`send_tcp_data` fix in `d70bb5b5`: `"48656c6c6f"` is simultaneously valid text and valid hex,
so sniffing cannot work and only the sender knows what it meant. Cache values are frequently
binary, so this matters here rather than being theoretical.

## Failure behaviour

Memcached clients block waiting for a reply, so silence hangs them until their own timeout.
When the model returns nothing usable, or the LLM call fails, `mod.rs` writes
`SERVER_ERROR ...` — the protocol's own way of saying this server failed. It never fabricates
a cache hit or a `STORED`, because inventing a successful store is the caching equivalent of
the OAuth2 fail-open.

Malformed commands get `CLIENT_ERROR <reason>`. Whether the connection continues depends on
whether the command is **self-delimiting**, and getting that wrong was a real defect:

- A retrieval or control line is one CRLF-terminated line, so the parser skips it
  (`Parsed::Invalid`) and the connection carries on.
- A **storage** command is `<header>\r\n<bytes octets>\r\n`. Rejecting the header while
  consuming only the header left `<bytes>` octets of attacker-chosen payload at the front of
  the buffer, where the next `parse_command` read them **as commands**:

  ```text
  set k notanumber 0 9\r\nversion\r\n\r\n
  ```

  answered `CLIENT_ERROR` for the header and then executed `version`. Anything a peer can put
  in a value it could put on the command path. `parse_storage` now parses `<bytes>` first and
  every rejection below it consumes the whole frame — which is what upstream's `conn_swallow`
  state does.
- Where `<bytes>` is missing, unparseable, or larger than `MAX_VALUE_LEN`, there is no
  boundary to skip to. That is `Parsed::Fatal`: reply and **close**. Guessing a boundary is
  the bug above; buffering an over-cap block purely to discard it is the allocation the cap
  exists to prevent. Logged `decision=connection_closed_unframable`.

`decision=` tags carry what the wire cannot. The text protocol defines exactly one
server-failure reply, so a refusal, an outage and a model that answered with nothing usable
are indistinguishable to the client; the log tags them `fail_closed_no_action`,
`fail_closed_llm_overloaded` and `fail_closed_llm_error`. The overload/unavailable split that
`redis` maps onto `-LOADING` vs `-ERR` has nowhere to go here — inventing a second reply would
be a protocol extension no client understands — so it lives only in the log.

## Concurrency

Each connection is one task that reads, parses, calls the LLM, and writes, strictly in order.
This is deliberately *not* the Idle/Processing/Accumulating machine other connection-oriented
protocols hand-roll: that machine exists to prevent two concurrent LLM calls on one
connection, and a single sequential task achieves the same thing by construction. Data
arriving mid-call waits in the socket buffer.

## Dashboard injection (`[ message this peer ]` / `[ disconnect this peer ]`)

Every connection registers a peer handle (`server::peer_support`) the moment it is accepted —
memcached has no greeting, so nothing needs to arrive first. `AppState::send_to_peer` runs the
action through the same executor as the LLM path, and the reader task and the peer task share
one `Arc<Mutex<WriteHalf>>`, so their writes never interleave. Every wire verb returns
`ActionResult::Output` (or `CloseConnection` for `close_memcached_connection`), so the generic
peer task covers the whole vocabulary with **no `Custom` gap**. The dashboard's disconnect
injects the generic `{"type":"close_connection"}`; `execute_action` has an explicit arm for it
(not advertised to the model) that returns `CloseConnection` so the write side half-closes and
the client reads EOF. The handle is removed on every exit path (EOF, read/write error, client
`quit`, model-requested close, oversize buffer) through the single cleanup in
`handle_connection`. Injected writes **are** counted: `src/server/peer_support.rs` calls `update_connection_stats`
on `ClientSendOutcome::Sent`. (This file previously claimed the opposite, and
`tests/server/redis/peer_inject_test.rs` asserts the counters move for exactly this path.)
Test: `tests/server/memcached/peer_inject_test.rs` (zero LLM calls).

## Ports and privilege

11211 is above 1023, so `privilege_requirement` is `None`. Declaring `PrivilegedPort(11211)`
would be dead code — the `svn`/`PrivilegedPort(3690)` mistake.

## Known limitations

- Text protocol only; no binary, no meta commands, no SASL authentication.
- `exptime` is reported to the model but nothing expires, because nothing is stored.
- `cas_unique` is reported and can be echoed, but uniqueness is not tracked; a `cas` conflict
  is whatever the model says it is.
- One LLM call per command. A client that pipelines a hundred `get`s costs a hundred calls;
  use a script handler for anything throughput-shaped.
- UDP memcached (the legacy `-U` mode) is not implemented.
