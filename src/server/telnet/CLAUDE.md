# Telnet Server Implementation

## Overview

A line-based text server on the Telnet port. Every byte the client sees is produced by a
handler — script, static or LLM. It is deliberately **Telnet-lite**: no option negotiation, no
terminal emulation, no filesystem behind it.

**Status**: Experimental
**Port**: 23 (privileged — `privilege_requirement` is `PrivilegedPort(23)`)
**Feature**: `telnet`
**Files**: `mod.rs` (accept loop, read loop), `actions.rs` (actions, events, metadata)

## What this is not

**IAC sequences are stripped, never answered.** RFC 854's `IAC WILL/WONT/DO/DONT`, and
subnegotiations (`IAC SB … IAC SE`), are recognised only well enough to be removed from the
data stream. The handler sees the text the user typed and nothing else; the client gets no
reply to its offers and therefore stays in its default line mode, which is the mode this
server can serve. A real `telnet(1)` client works in that mode; `nc` works too.

**This used to be worse than "not negotiated", and the difference is worth knowing.** The read
loop was `BufReader::read_line`, which validates UTF-8 across the whole line and returns
`InvalidData` when it fails. A real client opens with negotiation, whose bytes are not valid
UTF-8, so the first line the user typed arrived behind that junk, failed to decode, and the
loop treated the error as a reason to close — the connection died on the first line rather
than delivering the junk. `actions.rs` told the model those bytes "arrive as part of the first
message"; they never did. `tests/server/telnet/line_framing_test.rs` sends a real preamble and
asserts the handler received the typed line alone.

Stripping runs on the byte stream **before** lines are split out, and that ordering is load
bearing: an option byte can itself be `0x0A` (`IAC DO NAOCRD` is `FF FD 0A`), so a reader that
cut at newlines first would split a sequence in half. It also means a sequence straddling two
TCP reads is handled, because the machine's state survives between them.

The machine cannot be driven into unbounded state: its entire memory is one `IacState` plus a
buffer capped at `MAX_LINE_BYTES` (8 KiB), and a subnegotiation that never sends `IAC SE`
discards bytes as they arrive rather than accumulating them. **Lines are capped**, too — a
peer that streams bytes with no newline among them is answered `[netget] line too long` and
hung up on, where `read_line` previously grew its `String` for as long as the peer kept
sending. Nothing authenticates before that loop.

Also absent: character-at-a-time mode, server-side line editing, terminal type / window size
(`TTYPE`, `NAWS`), ANSI handling (colour codes are just bytes you emit), TLS or any encryption,
and any form of authentication that the handler does not invent for itself.

### Not a dependency, despite `Cargo.toml`

`Cargo.toml` has `telnet = ["nectar"]`, but no code in this module references `nectar` — the
codec is `tokio::io::BufReader::read_line`. The dependency is dead weight; removing it is an
edit to `Cargo.toml`, which this module does not own.

## Architecture

### Connection flow

1. Accept TCP (`create_reusable_tcp_listener`).
2. Register the connection in `ServerInstance` (with `ProtocolConnectionInfo::empty()` — there
   is no Telnet-specific connection info variant). Byte/packet counters and `last_activity` are
   updated on every read and write via `update_connection_stats`; they used to stay at zero,
   so the dashboard drew every telnet peer as `↓0 ↑0`.
3. Register a peer command channel (`server::peer_support`), so the dashboard's
   `[ message this peer ]` / `[ disconnect this peer ]` — and `AppState::send_to_peer` generally
   — can inject `send_telnet_*` / `close_connection` into this connection through the same
   executor the handlers use. The handle is removed on the close path.
4. If started with `send_first: true`, raise `telnet_connection_opened` and write the result —
   this is the only way to greet before the client types.
5. `TelnetLineReader::next_line` in a loop; per line, raise `telnet_message_received`, write
   the result. An oversize line is refused and closes the connection rather than raising it.
6. `close_connection` sets a flag that breaks the read loop and shuts the socket down.
7. Mark the connection closed in `ServerInstance`.

An idle telnet connection is **not** reaped: `AppState::cleanup_old_connections` only touches
protocols whose metadata declares `connectionless`, and Telnet does not. Before that scoping, a
peer whose line was parked more than 10s for a `manual` answer was evicted from state and shown
as closed while its socket was fine.

The accept-loop `JoinHandle` is registered with `AppState::register_server_task()`, so
`stop_server` aborts it and releases port 23. `spawn_with_llm_actions` propagates bind failure
with `?`.

Processing is strictly sequential: one line is fully handled before the next is read. There is
no Idle/Processing/Accumulating state machine and no `queued_data` — TCP backpressure does the
queueing. (Earlier revisions of this file described a state machine that does not exist.)

### Fixed failure modes

- `send_telnet_line` used to turn a `line` ending in `"\n"` into one ending in a lone `"\r"`,
  losing the line feed. It now always ends the line with exactly one CRLF.
- Log previews sliced `&text[..100]`, which panics when byte 100 falls inside a multi-byte
  UTF-8 character — reachable from any client that sends non-ASCII. Previews now truncate on a
  character boundary (`preview()` in `mod.rs`).
- `close_connection` broke out of the action loop only, leaving the read loop running and the
  socket open; it now closes the connection.
- Responses were round-tripped through `String::from_utf8_lossy` before being written, which
  silently replaced any non-UTF-8 byte with U+FFFD. Bytes are now written verbatim.
- `read_line` errors (non-UTF-8 input) were swallowed by `while let Ok(..)` and looked like a
  clean disconnect; they were then logged, and no longer happen at all — see the IAC note
  above, which is what was producing them.
- `read_line` buffered without limit, so an unauthenticated peer sending bytes with no newline
  among them made the server allocate for every one of them. Lines are now capped at 8 KiB.

## LLM Integration

`call_llm` is used for both events, so script and static handlers run in-process with **zero**
LLM calls (`call_llm` → `try_execute_event_handler`).

### Events

| Event                      | When                                              | Parameters |
|----------------------------|---------------------------------------------------|------------|
| `telnet_connection_opened` | on connect, **only if `send_first: true`**        | –          |
| `telnet_message_received`  | one complete line arrived                         | `message`  |

`message` is the line with its trailing CR/LF and surrounding whitespace trimmed.

### Actions

| Action                 | Bytes written                                  | Parameters          |
|------------------------|------------------------------------------------|---------------------|
| `send_telnet_message`  | `message` verbatim, nothing added               | `message` (required)|
| `send_telnet_line`     | `line` + exactly one CRLF                       | `line` (required)   |
| `send_telnet_prompt`   | `prompt` verbatim, no newline (default `"> "`)  | `prompt` (optional) |
| `wait_for_more`        | nothing — read the next line first              | –                   |
| `close_connection`     | nothing — closes the connection                 | –                   |

`send_telnet_message` is the escape hatch for exact control (e.g. a banner and prompt in one
write, or a byte sequence with no line ending). There are no async (user-triggered) actions.

### Startup parameters

| Parameter    | Type    | Effect                                              |
|--------------|---------|-----------------------------------------------------|
| `send_first` | boolean | `true` raises `telnet_connection_opened` on connect |

## Failure behaviour

Telnet has no status code, no framing and no transaction id — it is a byte stream with a human
on the other end — so on the wire it can say only two things: bytes, or nothing. Every `decision=`
distinction therefore has to live in the log, and three of the outcomes below are byte-for-byte
identical to the peer.

Nothing the server writes to the peer ever contains the error. `telnet_failure_notice` builds
its line from `crate::utils::WireFailure::classify(err).text()`, which returns `&'static str`
precisely so no value derived from an error can escape through it. This is the path that once
printed `[netget] cannot answer right now: ✗ LLM failed to generate valid response after
retries.` onto a stranger's terminal; `tests/wire_failure_test.rs` fails the build if the idiom
comes back.

| Outcome | On the wire | Log |
|---|---|---|
| Model answered `send_telnet_*` | those bytes | INFO `decision=model_answer` |
| Model answered `close_connection` with no reply | session closed, nothing written | INFO `decision=model_reject` |
| Model answered `wait_for_more` | nothing; session stays open | INFO `decision=model_wait_for_more` |
| Model answered with no usable action | nothing; session stays open | WARN `decision=model_silent` |
| An action could not be executed | nothing | ERROR `decision=fail_closed_bad_action` |
| Backend failed / saturated | `\r\n[netget] <category>\r\n` — a category, never the error | ERROR `decision=fail_closed_llm_error` / `decision=fail_closed_llm_overloaded` |
| Peer sent >8 KiB with no newline | `\r\n[netget] line too long\r\n`, then close | WARN `decision=refused_line_too_long` (no LLM call was made) |

The `send_first` greeting path carries the same tags, prefixed `Telnet greeting for …` instead
of `Telnet line from …`; `decision=model_silent` there means `send_first` was asked for and the
model produced no banner, so the peer stares at a blank screen with no notice.

Two invented tokens, both because the sanctioned set has no row for them:
`decision=model_wait_for_more` (a *deliberate* "nothing yet", which the wire cannot distinguish
from accidental silence) and `decision=refused_line_too_long` (the protocol refused before any
model call, so no `model_*` or `fail_closed_llm_*` token applies).

**Known rough edge, not repaired in the tagging pass**: `decision=model_silent` leaves the peer
with no notice at all and the session open, so a human sees a terminal that looks hung. That is
not a fail-open — nothing affirmative is asserted — but it is worse than the backend-failure
path, which at least says something. `tests/server/telnet/decision_tag_test.rs` pins the current
behaviour so changing it is deliberate.

## Storage

None. The protocol holds no session state, no filesystem and no user database. Anything that
looks like state — the current directory, a login prompt sequence, a command history — lives in
the handler's own memory.

## Testing

`tests/server/telnet/` holds four suites, all declared in its own `mod.rs` and all running.
(This section used to say the directory did not exist; it did, and `tests/server/mod.rs` had
declared it all along.)

| File | Covers |
|---|---|
| `test.rs` | echo, prompt, multiple lines on one connection, concurrent connections |
| `llm_failure_test.rs` | the notice written when the backend fails, that it carries no error text, and that the log carries `decision=fail_closed_llm_*` |
| `decision_tag_test.rs` | the two silent endings: `close_connection` → `decision=model_reject` and session closed; an empty answer → `decision=model_silent`, nothing written, session left open |
| `line_framing_test.rs` | a real `telnet(1)` negotiation preamble leaving the handler exactly the typed line, and an 8 KiB run with no newline being refused |

`line_framing_test.rs` asserts the *content* the handler received (`[hello]`), not merely that
a reply arrived — a static reply would have proved only that the connection survived.

`tests/client_handle_test.rs` additionally covers the dashboard flow end to end without a
model: a telnet server and a telnet client both on the `*` → manual rule, the human's answer to
the client's parked `telnet_connected` arriving as `telnet_message_received` on the server, the
connection surviving the idle sweep while that question waits, and the server's answer reaching
the client.

By hand, either client works:

```
telnet localhost 2323     # negotiation is stripped; line mode
nc localhost 2323
help
exit
```

## Example prompts

### Interactive shell

```
listen on port 2323 via telnet with send_first
On connection open send "NetGet 1.0\r\n$ "
help  -> list the commands: help, date, echo <text>, exit
date  -> the current date and time
echo  -> the text after the command
exit  -> "bye" then close_connection
Send "$ " after every response
```

### Line collector

```
listen on port 2323 via telnet
Buffer lines with wait_for_more until the client sends END
Then reply with the number of lines collected
```

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever. It now
declares both halves; the constants and the reasoning live beside them in
`src/server/telnet/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_LINE_READ_TIMEOUT` | 120s | Two minutes rather than a machine protocol's thirty seconds, because the other end is usually a *person*: they connect, read whatever banner `send_first` produced, and start typing. A scripted client sends immediately, so only someone who has not begun ever reaches this. The banner is generated and written before the read loop starts, so the model's time over it is outside the deadline by construction. |
| `IDLE_BETWEEN_LINES_TIMEOUT` | 600s | Cisco IOS's `exec-timeout 10 0` default for vty lines — the canonical idle bound for exactly the kind of device an operator points this server at, and therefore the number every telnet user already expects. A session here is long-lived by nature: a person thinks, reads output, and types again. |
| `MAX_CONNECTIONS` | 256 | Refusal: **a plain notice line**, `\r\n[netget] too many connections\r\n`. Telnet has no error frame — it is a byte stream with a human on the other end — so the protocol-appropriate refusal is the same fixed notice this file already writes for an over-long line. A terminal prints it; no client can mistake it for a prompt or a login success. |

**NetGet's own telnet client is the *connected-and-silent* case, and this bound is therefore
still short:** `src/client/telnet/mod.rs` writes nothing until a model action or a human's `[
send message ]` (its only unprompted write is a reactive IAC reply), so two minutes is less
than the 300s a `manual` rule gives that same person, and unlike `tcp` there is no
`first_byte_timeout_secs` for an operator to raise it with — `PROTOCOL_QUALITY.md`'s three-
state test.

**The deadline wraps the read and nothing else.** `TelnetLineReader::next_line` takes the bound
and applies it to the wait for *more bytes*, so a person on a slow link who is still typing keeps
the connection. The LLM round-trip and a `manual` rule parking a line for a human
(`src/state/intercepts.rs`, 300s by default) happen further down the loop, after a line has
already been read, so a slow answer can never be timed out from under itself.

`tests/server/telnet/connection_bounds_test.rs` drives all three from the wire: a silent peer is
closed at the first bound, a connection whose answer is parked for a human is not closed at all,
and the connection past `MAX_CONNECTIONS` is answered with the refusal above and then a clean
EOF. Each was verified by removing the thing it tests — the deadline, the busy marking, the cap —
and watching it fail. `tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound
is removed from the source, and `tests/accept_bounded_test.rs` covers the shared cap mechanism
itself, including that a busy connection is never reported as idle.
