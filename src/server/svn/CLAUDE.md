# SVN (Subversion) Protocol Implementation

Hand-rolled subset of the `svn://` wire protocol. The model answers each event
with a response tuple; the server owns the tuple syntax and the framing.

**State**: Beta — the real `svn` command-line client (1.14.5) completes
`svn info`, `svn ls` and `svn log` against it, in tests that fail rather than
skip when the binary is absent. **It cannot check out.** See
[What works, and where it stops](#what-works-and-where-it-stops) before quoting
the rating. **Privilege**: `None` — svn:// is port 3690, which is above 1024. It
previously declared `PrivilegedPort(3690)`, which could never fire (the preflight
only blocks ports below 1024) and read as protection that did not exist.
**Spec**:
[libsvn_ra_svn/protocol](https://svn.apache.org/repos/asf/subversion/trunk/subversion/libsvn_ra_svn/protocol).

## Protocol shape

svn is a tuple language, not a text protocol:

- lists — `( … )`
- words — bare tokens (`success`, `failure`, `dir`, `file`, `edit-pipeline`)
- numbers — bare digits
- **strings — counted**: `<byte-length>:<bytes>`, e.g. `5:trunk`. There is **no
  quoting**; `"trunk"` is not a string, it is five characters and two stray
  quote marks.

## Framing — `wire.rs`, and the defect it replaced

**A message's extent is decided by its own structure, never by a newline.** The
server read commands with `read_line` until September 2026, and the consequence
was that a real `svn` client could not get past its **own first message**:

1. NetGet writes the greeting.
2. The client parses it and replies with its capability tuple, which ends in a
   **space** and contains no newline anywhere.
3. `read_line` never returns. After `FIRST_COMMAND_READ_TIMEOUT` the server logs
   `sent nothing for 30s; closing idle connection` and hangs up.
4. `svn` reports `E210002: Network connection closed unexpectedly`.

So `svn info` and `svn log` failed too, not just `checkout` — before any command
was sent. Two things are worth carrying to the next protocol:

- **A counted string may contain any byte, newline included.** The client's
  `ANONYMOUS` token is base64 with a trailing `\n` inside the count, so even a
  reader that survived step 2 would desynchronise at step 4 of the handshake.
- **The suite could not see any of it.** `e2e_test.rs` writes
  `format!("{}\n", command)` itself, so it spoke a line protocol that only
  NetGet spoke. A protocol's own tests agreeing with it is not evidence.

`wire.rs` reads one whole `Item` — word, number, counted string or list —
**iteratively**, over an explicit stack of partial lists. Not out of taste: a
recursive descent over attacker-controlled nesting overflows the stack, and a
Rust stack overflow is a `SIGSEGV` against the guard page rather than a
catchable panic, so it takes the whole NetGet process rather than one connection
task. `src/utils/bencode.rs` is the same lesson in a different format.

| Bound | Value | Applied to |
|---|---|---|
| `MAX_TUPLE_DEPTH` | 64 | nested lists. `(` is one byte, so without it 64 KiB buys ~64 000 frames |
| `MAX_COMMAND_BYTES` | 64 KiB | the whole message |
| declared string length | the remaining budget | the number the peer **declares**, checked before anything is allocated — `1073741824:` is twelve bytes on the wire |

A refused message is answered with apr-err **210004**
`SVN_ERR_RA_SVN_MALFORMED_DATA` and a byte-literal string, then the connection
closes. Which bound fired goes to the log; the peer learns only that the message
was refused. `tests/server/svn/framing_test.rs` drives both bounds and both
framing cases from a raw socket, with zero LLM calls.

## The handshake

```text
server  ( success ( 2 2 ( ANONYMOUS ) ( edit-pipeline svndiff1 ) ) )      svn_greeting
client  ( 2 ( caps… ) 21:svn://host:port/lab 14:SVN/1.14.5 (…) ( ) )      svn_client_capabilities
server  ( success ( ( ANONYMOUS ) 3:lab ) )                              send_svn_auth_request
client  ( ANONYMOUS ( 33:YW5vbnltb3VzQGhvc3Q=\n ) )                      svn_auth_response
server  ( success ( ) )                                                  send_svn_auth_success
server  ( success ( 36:<uuid> 21:svn://host:port ( ) ) )                 send_svn_repos_info
client  ( get-latest-rev ( ) )                                           svn_command
server  ( success ( ( ) 0: ) ) ( success ( 42 ) )                        ← note the prefix
```

**Every command response in a completed session is two tuples.** ra_svn's client
calls `handle_auth_request` after *each* command: it reads one
`( success ( mechs realm ) )` and returns immediately, without replying, when the
mechanism list is empty. Omit that trivial auth-request and the client reads the
real answer where it expected this one and reports `E210004: Malformed network
data` — measured. NetGet writes it itself (`COMMAND_AUTH_PREFIX`) rather than
making an action emit it: it carries no decision and is the same bytes every
time, so putting it in an action would be making the model do framing. It goes
out **only** for a peer that completed the handshake, so a `nc` session or a
mocked test that never sends a capability tuple still sees exactly the bytes its
action produced.

**How the server tells the three client messages apart.** `( ANONYMOUS ( … ) )`
and `( get-dir ( … ) )` are the same shape, so it is mostly position:

- The capability tuple is the one message that is self-identifying — it opens
  with a **number** where every other client message opens with a word. A peer
  that never sends one goes straight to the command loop, which is what keeps
  `nc` and the mocked tests working.
- After it, the next message is the auth response — unless its head word
  contains a lower-case letter, in which case it is a command. Mechanism names
  come from the SASL registry and are upper-case; ra_svn commands are
  lower-case. That matters because a handler may answer with an **empty**
  mechanism list, which ra_svn reads as "no authentication required": the client
  then sends nothing back and its next message is already a command.

## What the model sees and controls

**Events**

| Event | When | Actions |
|---|---|---|
| `svn_greeting` | on connect, before anything is read | `send_svn_greeting`, `close_connection` |
| `svn_client_capabilities` | the client's answer to the greeting — `version`, `capabilities`, `url`, `ra_client` | `send_svn_auth_request`, `send_svn_failure`, `close_connection` |
| `svn_auth_response` | its choice of mechanism — `mechanism`, `token`, and `url` carried forward | `send_svn_auth_success`, `send_svn_repos_info`, `send_svn_failure`, `close_connection` |
| `svn_command` | every command — `command_line`, `command`, `args` | `send_svn_success`, `send_svn_failure`, `send_svn_list`, `send_svn_stat`, `send_svn_response`, `close_connection` |

`args` is the command's **params list, unwrapped**: ra_svn spells a command
`( name ( params… ) )`, so leaving it wrapped would make every argument index one
deeper than the protocol document says. A tuple that is not that shape keeps its
remaining elements as they are.

**Actions and what they put on the wire**

| Action | Output |
|---|---|
| `send_svn_greeting` | `( success ( min max ( mechanisms… ) ( edit-pipeline svndiff1 ) ) )` |
| `send_svn_auth_request` | `( success ( ( mechanisms… ) <realm> ) )` |
| `send_svn_auth_success` | `( success ( [ token ] ) )` |
| `send_svn_repos_info` | `( success ( <uuid> <root> ( capabilities… ) ) )` |
| `send_svn_success` | `( success ( <items> ) )` |
| `send_svn_failure` | `( failure ( ( code <message-string> <file-string> 0 ) ) )` |
| `send_svn_list` | `( success ( 0 ( ) ( ( name kind size false rev ( ) ( ) ) … ) ) )` |
| `send_svn_stat` | `( success ( ( ( kind size has-props rev ( date ) ( author ) ) ) ) )`, or `( success ( ( ) ) )` for `kind: "none"` |
| `send_svn_response` | the raw string, newline-terminated — the escape hatch |

**Count the parens on `send_svn_stat`.** The response params are `( ? entry )` —
a sub-tuple holding an optional list — so the entry sits three levels in. One
level short and the client answers `E210004`, which is how this was found.

**Strings are encoded by the server.** `send_svn_list` names,
`send_svn_failure` messages, realms, UUIDs and root URLs all become counted
strings; in `send_svn_success`, a value made only of digits is emitted as a
number (revisions) and anything else as a counted string. The model never has to
count bytes.

`send_svn_list` previously emitted an opening paren for the first entry and a
closing paren only for later ones — every listing was unbalanced — with
`"quoted"` names and a `rev:N` pseudo-token. Nothing that reads svn could parse
any of it. It is now byte-for-byte what `svn ls` accepts.

**Authentication is a decision, and it fails closed.** Nothing in `actions.rs`
can synthesise an acceptance: `send_svn_auth_success` is the only thing that
emits one, refusal is `send_svn_failure`, and the two share no code path — so a
handler that answers nothing is logged `decision=no_action` and writes nothing,
which the client reads as a stall rather than as approval. What the server does
**not** do is check any credential against anything: the token reaches the model
and the model decides. Say that out loud rather than implying a check exists.

### Dashboard injection (peer messaging)

Each live connection registers a peer handle (`peer_support`), so the dashboard's
`[ message this peer ]` / `[ disconnect this peer ]` rows work. An injected action
runs through the same executor and `SvnProtocol::execute_action` the model's does,
so an injected `send_svn_success` / `send_svn_list` / `send_svn_response` is
encoded identically. All svn wire verbs return `ActionResult::Output` (none return
`ActionResult::Custom`), so the generic peer task needs no bespoke arm.
`{"type":"close_connection"}` (the disconnect row) returns
`ActionResult::CloseConnection`, which half-closes the write side; the peer reads
EOF and the reader's clean-EOF path runs the normal teardown.

The connection is split into an owned read half and an `Arc<Mutex<WriteHalf>>`
shared by the reader and the peer task, and the peer handle is dropped on **every**
exit path (greeting write/LLM failure, command write failure, EOF, read error,
close_connection, command LLM failure). `update_connection_stats` is called on
every read and every write, so the rail's `↓ ↑` counters and `last_activity` stay
live. An item's trailing separator is drained from what has **already arrived**
(never by awaiting more), so the inbound counter is not one byte short for as long
as the peer stays quiet.

### Failure behavior

ra_svn lets the server answer the greeting *or* any command with
`( failure ( ( apr-err message file line ) ) )`, so that is what a backend
failure produces — the protocol's own error shape, not silence and not an
invented one.

| Outcome | Wire | Log |
|---|---|---|
| Handler wrote a response | that response | — |
| Handler returned `close_connection` | half-close (EOF) | `decision=model_close` |
| Handler returned no action | nothing; connection stays open | `decision=no_action` |
| LLM/handler call errored | `( failure ( ( <code> <category> 0: 0 ) ) )` then close | `decision=fail_closed_llm_error category=…` + the error |
| Message refused by the framing bounds | `( failure ( ( 210004 … ) ) )` then close | `decision=fail_closed_framing` + which bound |

The two [`WireFailure`](../../utils/wire_failure.rs) categories map onto
different apr error numbers so a client can back off rather than record a
permanent fault: `Overloaded` → **210003** `SVN_ERR_RA_SVN_IO_ERROR` (transient),
`Unavailable` → **210000** `SVN_ERR_RA_SVN_CMD_ERR` (generic). The message is
`WireFailure::text()`, a `&'static str` — the backend URL, the model name, file
paths and anyhow chains stay in the log and the status stream, never on the wire.
In a completed session the failure tuple carries the trivial auth-request prefix
too, or the client reads the refusal as that auth-request and reports malformed
data instead of the reason it was sent.

"Answered with nothing" is deliberately *not* turned into a failure: a static
handler with an empty action list, or a human choosing "Answer with nothing" at
the dashboard before injecting bytes through `[ message this peer ]`, is a real
answer. It is logged distinctly so an operator can tell it from a backend error.

`tests/server/svn/llm_failure_test.rs` points the server at a dead backend and
asserts the peer reads a well-formed failure tuple carrying only a category, then
EOF.

## What works, and where it stops

Measured against Subversion 1.14.5, and asserted in
`tests/server/svn/real_client_test.rs`:

| Command | Works | What it needs from the handler |
|---|---|---|
| `svn info` | yes | `get-latest-rev`, `stat`, `get-lock` |
| `svn ls` | yes | `get-latest-rev`, `stat`, `get-dir` |
| `svn log` | yes | `get-latest-rev`, `log` — a **stream**: bare log-entry tuples, then the word `done`, then the command response. No action models that; it goes through `send_svn_response` |
| `svn checkout` / `update` / `switch` | **no** | the editor/report command set and svndiff, none of which exists here |
| `svn commit` | **no** | the same, plus a repository |

That gap is the reason the Beta rating is written with its limit attached: the
client can browse, and it cannot get a working tree. `get-lock` is the other
thing worth knowing — `svn info` asks for it and there is no action for it, so
the empty optional `( success ( ( ) ) )` goes through `send_svn_response`.

## Not implemented

svndiff / delta transfer, the editor commands used by checkout and commit,
`REPORT`-style update flows, SASL (mechanisms are announced and the token is
handed to the model; nothing is verified), locking, merge tracking, repository
administration, protocol versions other than 2, and any repository storage — the
model answers every command, nothing is stored.

## Example prompts

```json
{"type": "open_server", "port": 3690, "base_stack": "svn",
 "event_handlers": [
   {"event_pattern": "svn_greeting", "handler": {"type": "static",
     "actions": [{"type": "send_svn_greeting", "mechanisms": ["ANONYMOUS"]}]}},
   {"event_pattern": "svn_client_capabilities", "handler": {"type": "static",
     "actions": [{"type": "send_svn_auth_request", "mechanisms": ["ANONYMOUS"],
                  "realm": "lab"}]}},
   {"event_pattern": "svn_auth_response", "handler": {"type": "static",
     "actions": [{"type": "send_svn_auth_success"},
                 {"type": "send_svn_repos_info",
                  "uuid": "8f3c1d2e-4b5a-4c6d-9e7f-0a1b2c3d4e5f",
                  "repository_root": "svn://127.0.0.1:3690"}]}},
   {"event_pattern": "svn_command", "handler": {"type": "static",
     "actions": [{"type": "send_svn_list", "items": [
       {"name": "trunk", "kind": "dir", "revision": 1},
       {"name": "README.txt", "kind": "file", "size": 1234, "revision": 5}]}]}}]}
```

A static `repository_root` only works when the port is fixed, as above. On an
ephemeral port the root has to come from the `url` the `svn_auth_response` event
carries — which is what a model, or a script handler, would read it from.

```
listen on port 3690 via svn. Fake repository with the standard trunk/branches/tags
layout, latest revision 42. Offer ANONYMOUS, accept it, and answer get-latest-rev
with 42, stat with a directory, and get-dir with the three top-level entries.
```

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever, and a
hundred of them was a free denial of service on a server that would happily accept a hundred
more. It now declares both halves; the constants and the reasoning live beside them in
`src/server/svn/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_COMMAND_READ_TIMEOUT` | 30s | `svnserve` speaks first: the greeting goes out and a real `svn` client answers with its capabilities immediately — it has nothing to decide and nobody to ask. |
| `IDLE_BETWEEN_COMMANDS_TIMEOUT` | 180s | ra_svn after the greeting is strictly request/response, so seconds of silence normally means the client is gone. The exception, and the reason this is minutes, is that `svn` prompts for credentials on the user's terminal *mid-session*: a human typing a password is a legitimate multi-minute pause with the connection live and nothing on the wire. |
| `MAX_CONNECTIONS` | 256 | Refusal: **ra_svn's own `( failure ( ( 210003 … ) ) )` tuple** — apr-err 210003, the code this file already uses for "at capacity" rather than "your request was wrong". A client reading it where it expected a greeting reports malformed data, which is a real limit of speaking before the greeting; what it buys is an operator, a packet capture and a `nc` session that can all see the reason in the bytes. |

**The deadline covers the read and nothing else.** The deadline wraps the `read_item()` call in
this protocol's own loop, and everything that can legitimately take minutes happens after it
returns. The LLM round-trip, and a `manual`
rule parking an event for a human (`src/state/intercepts.rs`, 300s by default), are outside
every deadline here, so an answer that takes minutes can never close the connection it is an
answer for. That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse:
TFTP evicted live transfers because "idle" was measured wrongly.

`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is removed;
`tests/accept_bounded_test.rs` drives the shared helper, including the guarantee that a busy
connection is never reported as idle.
