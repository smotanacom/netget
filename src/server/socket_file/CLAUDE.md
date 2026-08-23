# Socket File (Unix Domain Socket) Protocol Implementation

Raw byte-stream server over a Unix domain socket. The model sees exactly the
bytes the client sent and decides exactly what bytes go back — the same contract
as `tcp`, addressed by filesystem path instead of IP:port.

**State**: Experimental. **Platform**: Unix only; the whole module is
`#![cfg(unix)]`. **Privilege**: none — access is controlled by filesystem
permissions on the socket file, not by port numbers.

## Startup

| Parameter | Required | Meaning |
|---|---|---|
| `socket_path` | yes | path to create, e.g. `./netget.sock` |
| `send_first` | no | send a banner on connect (raises `socket_file_connection_opened`) |

No port is involved. `default_binding()` returns empty binding defaults so that
`server_startup.rs` does not demand one; without that declaration this protocol
could not be started over MCP at all ("requires 'port' parameter").

**A stale socket is removed only if it really is a socket.** `socket_path` comes
from the model or an MCP caller, so an unconditional `remove_file` is an
arbitrary-file delete — a typo'd or hostile path would unlink whatever is there
before binding. The server stats the path with `symlink_metadata` (which does not
follow symlinks) and refuses to start unless it is a socket, naming what it found.

## What the model sees and controls

**Events**

- `socket_file_connection_opened` — only when `send_first` is set. No fields.
- `socket_file_data_received` — `data` plus `encoding`.

**Actions**: `send_socket_data` (`data`, `encoding`), `wait_for_more`,
`close_this_connection` (`close_connection` is accepted as an alias).

There are no async actions. `send_to_connection`, `close_connection` and
`list_connections` used to be advertised as async actions backed by a second,
always-empty connection map inside `SocketFileProtocol`; nothing ever inserted
into it and nothing routed their output to a connection, so all three were
inert. The server owns the only connection map, because it holds the write
halves.

### Encoding is explicit in both directions

Inbound: `data` is the received bytes as text when every byte is printable
ASCII, hex otherwise, and `encoding` (`"utf8"` / `"hex"`) says which.

Outbound: `send_socket_data` reads the same `encoding` field, defaulting to
`"utf8"`. There is deliberately no auto-detection — `"48656c6c6f"` is both valid
text and valid hex — so echoing a payload back unchanged means passing back the
same `data` *and* `encoding`. Invalid hex is an action error, not a panic.

This mirrors the fix made to `send_tcp_data`; before it, the docs advertised hex
support that the executor did not implement, and a model following them put
literal ASCII on the wire.

## Connection handling

Per connection: `ReadHalf` owned by a reader task, `WriteHalf` behind
`Arc<Mutex<…>>` in the shared map, and an Idle → Processing → Accumulating state
machine that serialises LLM calls and queues data that arrives mid-call.

**The connection is registered in the accept loop, before either task is
spawned.** It used to be registered by the banner task, which raced the reader:
`handle_data_with_actions` returns silently when the connection is not in the
map, so a client that wrote immediately after connecting
(`printf ping | nc -U …`) had its first payload dropped with no response and no
log line. Verified: before the fix the log stopped at "received 5 bytes"; after
it, the handler runs.

Dual logging throughout: DEBUG summaries with a 100-character preview, TRACE full
payloads (text as a string, binary as hex), to both `netget.log` and the TUI.

### When the LLM call fails

A raw byte stream has no error frame, so there is nothing safe to *say*: the only
honest signal is FIN. Both LLM entry points (the `send_first` banner and
`socket_file_data_received`) classify the error with
`crate::utils::WireFailure`, log the full error to `netget.log` and the status
stream, then `shutdown()` the write half, drop the connection from the map and
call `close_connection_on_server`. The peer's next read returns EOF immediately
instead of blocking until its own timeout. Nothing derived from the error — the
backend URL, the model name, an `anyhow` chain — ever reaches the socket.

Three outcomes stay distinguishable in the log:

| `decision=` | meaning |
|---|---|
| `model_close` | the model answered by hanging up |
| `model_no_actions` | the model answered with no bytes (legitimate; connection stays open) |
| `fail_closed_llm_error` | the LLM call errored; `class=overloaded\|unavailable` |

## Not implemented

Peer credentials (`SO_PEERCRED` — a Unix socket can identify the connecting
process; the model is not told), idle timeouts, backpressure, and any
bound on how much `wait_for_more` accumulates. `SocketAddr` is required by
internal APIs, so connections report the placeholder `127.0.0.1:0`; the real path
is in `protocol_info.socket_path`.

## Example prompts

```json
{"type": "start_server", "protocol": "socket_file",
 "startup_params": {"socket_path": "/tmp/nge.sock"},
 "event_handlers": [{"event_pattern": "socket_file_data_received",
   "handler": {"type": "static", "actions": [{"type": "send_socket_data", "data": "ACK\n"}]}}]}
```

```
Create socket file at ./echo.sock and echo back any data received, passing the
same encoding you were given.
```

```
Listen on ./greeter.sock with send_first=true. Send "READY\n" on connect, then
answer HELLO with "GREETINGS\n" and QUIT with "BYE\n" followed by
close_this_connection.
```

## Verified

Started over MCP with `startup_params.socket_path`, with static handlers (zero
LLM calls):

- A Python `AF_UNIX` client that writes immediately after connect gets the reply
  (this is the race described above).
- `{"data": "48454c4c4f0a", "encoding": "hex"}` puts `HELLO\n` on the wire.
- Pointing `socket_path` at a regular file is refused — "it exists but is not a
  Unix domain socket (it is a regular file)" — and the file is left intact.
- `nc -U` works, but macOS `nc` exits on stdin EOF, so
  `printf ping | nc -U …` can miss the reply; keep stdin open or use a real
  client.

`tests/server/socket_file/` is declared in `tests/server/mod.rs` and runs against
the mock LLM.
