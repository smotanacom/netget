# Redis Protocol Implementation

RESP2 server. `redis-protocol` v6.0 parses inbound frames; responses are encoded
by hand in `actions.rs` (`encode_*`, the single source of truth — each reply verb's
executor returns the encoded bytes as `ActionResult::Output`). The LLM owns every
reply — there is no key space, no storage, and no command dispatch table in Rust.

**State**: Beta — **two** independent clients, sharing no code with each other or
with the `redis-protocol` crate the server parses with:

- **redis-rs** (Rust) in `tests/server/redis/e2e_test.rs`: six tests, one per
  RESP2 reply type, each deserialised into the Rust type the test asked for.
- **redis-cli** (C; the binary here is `valkey-cli`, the redis-cli-compatible
  fork) in `real_client_test.rs`: seven commands on one connection, asserted as
  one ordered list of what `--no-raw` printed. That reads the type off the wire
  rather than coercing it — a bulk string is quoted, an integer is
  `(integer) n`, a nil is `(nil)` and not an empty line, an error is `(error) …`
  — and because it is one ordered list, a reply landing against the wrong command
  fails as a mismatched line rather than passing as a same-typed value. It
  hard-fails when the binary is absent.

Not Stable: Stable additionally wants spec compliance and scripting support
reviewed, which has not been done. (This file used to say Experimental while
`actions.rs` said Beta — the code was right.)
**Port**: 6379 by default. **Privilege**: `None` (6379 > 1024).
**Stack**: `ETH>IP>TCP>Redis`. **Spec**: RESP2.

## What the model sees and controls

**Event**: `redis_command`, one per decoded RESP frame.

| Field | Notes |
|---|---|
| `command` | the frame flattened to a single space-separated string, e.g. `SET mykey hello` |

That string is all the model gets — arguments are not split out, and there is no
argument-count or key field. A command whose argument contains a space is
indistinguishable from two arguments.

**Actions** (all sync; there are no async actions):

| Action | Parameters | Wire form |
|---|---|---|
| `redis_simple_string` | `value` (required) | `+value\r\n` |
| `redis_bulk_string` | `value` (optional; `null` ⇒ nil) | `$len\r\n…\r\n` |
| `redis_integer` | `value` (required, i64) | `:value\r\n` |
| `redis_array` | `values` (required, array) | `*n\r\n…` |
| `redis_error` | `message` (required) | `-message\r\n` |
| `redis_null` | — | `$-1\r\n` |
| `close_this_connection` | — | flushes pending output, then closes |

`redis_array` element mapping, exactly as implemented in `actions.rs::encode_array`:

| JSON element | RESP2 |
|---|---|
| string | bulk string |
| integer | RESP integer (`:42\r\n`), **not** a bulk string |
| float | bulk string of its text form |
| `true` / `false` | bulk string `"1"` / `"0"` |
| `null` | nil bulk string |
| array / object | its JSON text in a bulk string |

Verified with `redis-cli --no-raw`: `["k1", 42, true, null, {"a":1}]` returns
`"k1"`, `(integer) 42`, `"1"`, `(nil)`, `"{\"a\":1}"`.

### CR and LF in a model-supplied payload

A RESP **simple** string (`+…`) and **simple error** (`-…`) are CRLF-terminated
with no length prefix, so a newline anywhere in the payload ends the frame early
and everything after it is parsed as the *next* reply. The connection is then
desynchronised permanently: every later command reads the previous one's
leftovers, and the client has no way to notice.

`redis_simple_string`'s `value` and `redis_error`'s `message` come from model
output, so `encode_simple_string` and `encode_error` map each CR and LF to a
space — which is exactly what Redis itself does
(`addReplyErrorFormat` runs `sdsmapchars(s, "\r\n", "  ", 2)`). The text
survives; only its ability to end the frame does not. A WARN names the action.

This mattered most for `redis_error`: a model asked to explain a refusal writes
multi-line prose without thinking about framing. `mod.rs` had documented the
hazard on its **own** LLM-failure path and avoided it by sending a fixed
category, but the model-facing verbs had no guard at all.

Bulk strings, arrays and integers are unaffected — a bulk string is
length-prefixed, so it can carry any bytes, newlines included.

### Failure behavior

- **No response action** → `-ERR no response produced for this command`. (Redis
  is strictly request/response; staying silent would hang the client until its
  own timeout.)
- **LLM call fails** → `-LOADING <category>` when
  `crate::llm::is_overload_error` says the backend was merely saturated,
  otherwise `-ERR <category>`. Clients already treat `LOADING` as "not ready
  yet, retry" and several retry it automatically, so an outage does not get
  recorded as a permanent fault. **The text is a
  `crate::utils::WireFailure` category, never the error itself** — see
  `redis_error_message`. Logged `decision=fail_closed_llm_overloaded` /
  `decision=fail_closed_llm_error`.
- **Model chose `redis_error`** → its own message, logged
  `decision=model_reject`. On the wire a refusal and an outage are both just
  `-…`, so as in `src/server/radius/` only the log distinguishes them; the
  no-response case above is `decision=fail_closed_no_action`.
- **Action result that is `Custom` rather than `Output`** → logged at WARN and
  skipped (no Redis action returns `Custom` any more; the arm exists so a
  regression is loud); if nothing else was produced the no-response error above
  is sent.
- **Undecodable RESP** → the connection is closed.
- **Incomplete frame larger than 64 MB** (`MAX_PENDING_FRAME_BYTES`) → an error
  is sent and the connection closed. Without this cap a client announcing
  `$2000000000\r\n` and stalling would grow the per-connection buffer without
  bound.

## Architecture

- `spawn_with_llm_actions` binds with `?`, so a bind failure surfaces as
  `ServerStatus::Error` rather than a phantom `Running`, and registers the
  accept-loop `JoinHandle` via `AppState::register_server_task()` so
  `stop_server` releases the socket.
- One task per connection, **registered with `register_server_task` as well**.
  Registering only the accept loop released the port on `stop_server` while
  leaving every in-flight session running — the socket vanished from `netstat`
  and the peer was still being served.
  `tests/server/redis/resp_framing_test.rs` pins this.
  `handle_connection` wraps `run` so the connection is always marked `Closed` in
  `AppState` on exit; `update_connection_stats` is called for bytes/packets in
  both directions.
- Read into a `Vec`, `decode()` frames off the front, drain what was consumed.
  Multiple pipelined frames in one read are processed in order, each with its own
  LLM call.
- Reply verbs are encoded in `execute_action` (`actions.rs`), which returns the
  RESP bytes as `ActionResult::Output`. The read loop only concatenates `Output`
  bytes (flattening `Multiple`) and writes them; it encodes nothing itself except
  the three errors it synthesises (frame cap, LLM failure, no-response), which
  call the same `actions::encode_error`.
- Responses for one command are accumulated into a single buffer and written
  once, so a `close_this_connection` issued alongside a reply still flushes.
- No per-connection state machine: commands on one connection are handled
  strictly sequentially by the read loop, so concurrent LLM calls cannot happen.

### Dashboard injection (`[ message this peer ]` / `[ disconnect this peer ]`)

Every connection registers a peer handle (`server::peer_support`) before its first read,
so a manual `*` rule parking the first command still leaves the operator able to reach
it. The stream is `tokio::io::split` and the write half is an `Arc<Mutex<..>>` shared
with the generic peer command task; the handle is removed on every exit path through the
single cleanup in `handle_connection`. Counters (`update_connection_stats`) move on every
read and every write, including the frame-cap error.

**The whole vocabulary is injectable.** All six reply verbs (`redis_simple_string`,
`redis_bulk_string`, `redis_array`, `redis_integer`, `redis_error`, `redis_null`) are
RESP-encoded inside `execute_action` and return `ActionResult::Output`, which the generic
peer task writes to the connection's write half — so an injected reply verb puts exactly
the same bytes on the wire as the read-loop path (they share the `encode_*` functions in
`actions.rs`). `close_connection` (what "disconnect this peer" sends; an explicit arm in
`execute_action`, not offered to the model — its verb is `close_this_connection`)
half-closes and the client reads EOF. This used to be the "Custom-result gap": the verbs
returned `Custom` and only the read loop could encode them, so injection reported
`Executed` without writing. `tests/server/redis/peer_inject_test.rs` pins the fixed
behavior (`Sent`, bytes at the client socket, counters moved).

Injected replies are unsolicited from the client's point of view: `redis-cli` sitting at
its prompt will parse the frame as the reply to its *next* command. That is inherent to
injecting into a strictly request/response protocol, not a NetGet bug.

## Not implemented

- **RESP3** — no `HELLO 3`, push messages, doubles, maps or sets.
- **Inline commands** (`PING\r\n` typed into `nc`) — only RESP arrays decode;
  anything else closes the connection. `redis-cli` and `redis-rs` always send
  RESP arrays, so this only affects hand-typed sessions.
- **AUTH / SELECT / MULTI / EXEC / WATCH** — reach the model as ordinary
  commands with no special handling. There is no auth gate.
- **Pub/sub**, **blocking commands** (`BLPOP`), **Lua** (`EVAL`), **cluster**,
  **replication**, **persistence**.
- **TLS**.
- Server-initiated pushes: nothing can be written except in reply to a command.

## Testing

Five files, declared in `tests/server/redis/mod.rs`:

- `e2e_test.rs` — six `redis-rs` tests, one per RESP2 reply type.
- `real_client_test.rs` — the real `redis-cli` binary, seven commands on one
  connection, asserted as one ordered list of rendered replies. Hard-fails when
  the binary is missing. Together with `e2e_test.rs` this is what the Beta rating
  rests on.
- `resp_framing_test.rs` — CR/LF in a model-supplied simple string or error
  cannot split the frame, and `stop_server` ends an in-flight connection rather
  than just the listener. Zero LLM calls.
- `llm_failure_test.rs` — the RESP error a client sees when the backend fails.
- `peer_inject_test.rs` — dashboard injection through `send_to_peer`.

`--test` names a **target**, so `--test server::redis::e2e_test` makes cargo list
its targets and exit having run nothing — it does not fail, so it silently looks
like a pass. Filter after `--` instead:

```bash
./cargo-isolated.sh test --no-default-features --features redis \
    --test server -- server::redis --test-threads=100
```

Real-client checks used during review, via `--mcp-http` with a static handler so
no model is involved:

```bash
netget --mcp-http 18899 &
# start_server protocol=redis port=16379 event_handlers=[{redis_command → static redis_array}]
redis-cli -p 16379 --no-raw KEYS '*'
```

## Example prompts

```
Start a Redis server on port 6379. Reply PONG to PING with redis_simple_string.
For GET on a key you have not seen, use redis_null. For SET, reply OK.
```

Prefer a script or static handler for anything deterministic — every command
otherwise costs one model round-trip.

## References

- [RESP2 specification](https://redis.io/docs/reference/protocol-spec/)
- [redis-protocol crate](https://docs.rs/redis-protocol/)
- [redis-rs](https://docs.rs/redis/) — used by the E2E tests

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever, and a
hundred of them was a free denial of service on a server that would happily accept a hundred
more. It now declares both halves; the constants and the reasoning live beside them in
`src/server/redis/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_COMMAND_READ_TIMEOUT` | **300s**, overridable per server with `first_byte_timeout_secs` | Was 30s, on the argument that every real RESP client speaks immediately — `redis-cli` sends `COMMAND DOCS`, `redis-rs` sends `PING` or its configured `HELLO`/`AUTH`. True of every third-party client and **false of the one this server most often has.** NetGet's own Redis client (`src/client/redis/mod.rs`) is a bare `TcpStream::connect` that writes nothing until an action says to, and a client made with the dashboard's `[ + redis client ]` is routed `*` → manual: it connects, is answered with nothing, and waits for a person to type into `[ send message ]`. At 30s the server dropped it while the operator was still looking at it — the `tcp` defect, same shape. 300s is the window a `manual` rule gives a human (`src/state/intercepts.rs`) and is this server's own idle bound, so a hand-driven session is bounded the same way before its first command as after it. Cost: one idle stranger holds a slot for 300s rather than 30s, still capped at `MAX_CONNECTIONS` and still answered above that cap. A listener exposed to strangers should set the parameter low; 30 remains a sound choice for one. |
| `IDLE_BETWEEN_COMMANDS_TIMEOUT` | 300s, overridable per server with `idle_timeout_secs` | Real Redis ships `timeout 0` — it never closes an idle client — and every pooling client in the ecosystem depends on holding an established connection unused between bursts. Closing those at 30s would break correct clients to fix a problem they are not causing. |
| `MAX_CONNECTIONS` | 256 | Refusal: **`-ERR max number of clients reached`**, byte for byte what real Redis sends in the same situation. `redis-rs` surfaces it as `ResponseError` and `redis-cli` prints it; a simple error can never be mistaken for data, which is what makes it safe unprompted. |

**The deadline covers the read and nothing else.** The deadline wraps the `read()` call in this protocol's own loop, and everything that can legitimately take minutes happens after it returns. The LLM round-trip, and a `manual`
rule parking an event for a human (`src/state/intercepts.rs`, 300s by default), are outside
every deadline here, so an answer that takes minutes can never close the connection it is an
answer for. That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse:
TFTP evicted live transfers because "idle" was measured wrongly.

Both bounds are declared startup parameters, because the right value is a property of who is
on the other end and only the operator knows that.
`tests/server/redis/connection_bounds_test.rs` drives all three claims from a raw socket: a
short `first_byte_timeout_secs` closes a silent peer, a short `idle_timeout_secs` closes one
that has been answered (proving the loop switches bounds), and the **default** leaves a silent
peer alone for 40 seconds — past the 30 this used to be, which is the regression for the defect
above and the reason that one test is deliberately slow.

`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is removed;
`tests/accept_bounded_test.rs` drives the shared helper, including the guarantee that a busy
connection is never reported as idle.
