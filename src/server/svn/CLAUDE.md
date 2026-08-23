# SVN (Subversion) Protocol Implementation

Hand-rolled subset of the `svn://` wire protocol. The model answers each command
with a response tuple; the server owns the tuple syntax.

**State**: Experimental. **Privilege**: `None` — svn:// is port 3690, which is
above 1024. It previously declared `PrivilegedPort(3690)`, which could never
fire (the preflight only blocks ports below 1024) and read as protection that did
not exist. **Spec**:
[libsvn_ra_svn/protocol](https://svn.apache.org/repos/asf/subversion/trunk/subversion/libsvn_ra_svn/protocol).

## Protocol shape

svn is a tuple language, not a text protocol:

- lists — `( … )`
- words — bare tokens (`success`, `failure`, `dir`, `file`, `edit-pipeline`)
- numbers — bare digits
- **strings — counted**: `<byte-length>:<bytes>`, e.g. `5:trunk`. There is **no
  quoting**; `"trunk"` is not a string, it is five characters and two stray
  quote marks.

Exchange: the server greets, the client answers with its capabilities and the
repository URL, then commands and responses alternate.

## What the model sees and controls

**Events**

- `svn_greeting` — raised on connect, before anything is read. Actions:
  `send_svn_greeting`, `close_connection`.
- `svn_command` — `command_line`, `command`, `args`. Actions:
  `send_svn_success`, `send_svn_failure`, `send_svn_list`, `send_svn_response`,
  `close_connection`.

**Actions and what they put on the wire**

| Action | Output |
|---|---|
| `send_svn_greeting` | `( success ( min max ( mechanisms… ) ( edit-pipeline svndiff1 ) ) )` |
| `send_svn_success` | `( success ( <items> ) )` |
| `send_svn_failure` | `( failure ( ( code <message-string> <file-string> 0 ) ) )` |
| `send_svn_list` | `( success ( 0 ( ) ( ( name kind size false rev ( ) ( ) ) … ) ) )` |
| `send_svn_response` | the raw string, newline-terminated — the escape hatch |

**Strings are encoded by the server.** `send_svn_list` names and
`send_svn_failure` messages become counted strings; in `send_svn_success`, a
value made only of digits is emitted as a number (revisions) and anything else as
a counted string. The model never has to count bytes.

`send_svn_list` previously emitted an opening paren for the first entry and a
closing paren only for later ones — every listing was unbalanced — with
`"quoted"` names and a `rev:N` pseudo-token. Nothing that reads svn could parse
any of it.

### Dashboard injection (peer messaging)

Each live connection registers a peer handle (`peer_support`), so the dashboard's
`[ message this peer ]` / `[ disconnect this peer ]` rows work. An injected action
runs through the same executor and `SvnProtocol::execute_action` the model's does,
so an injected `send_svn_success` / `send_svn_list` / `send_svn_response` is
encoded identically. All svn wire verbs return `ActionResult::Output` (none return
`ActionResult::Custom`), so the generic peer task needs no bespoke arm.
`{"type":"close_connection"}` (the disconnect row) returns
`ActionResult::CloseConnection`, which half-closes the write side; the peer reads
EOF and the reader's `read_line == 0` path runs the normal teardown.

The connection is split into an owned read half and an `Arc<Mutex<WriteHalf>>`
shared by the reader and the peer task, and the peer handle is dropped on **every**
exit path (greeting write/LLM failure, command write failure, EOF, read error,
close_connection, command LLM failure). `update_connection_stats` is called on
every read and every write, so the rail's `↓ ↑` counters and `last_activity` stay
live.

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

The two [`WireFailure`](../../utils/wire_failure.rs) categories map onto
different apr error numbers so a client can back off rather than record a
permanent fault: `Overloaded` → **210003** `SVN_ERR_RA_SVN_IO_ERROR` (transient),
`Unavailable` → **210000** `SVN_ERR_RA_SVN_CMD_ERR` (generic). The message is
`WireFailure::text()`, a `&'static str` — the backend URL, the model name, file
paths and anyhow chains stay in the log and the status stream, never on the wire.

"Answered with nothing" is deliberately *not* turned into a failure: a static
handler with an empty action list, or a human choosing "Answer with nothing" at
the dashboard before injecting bytes through `[ message this peer ]`, is a real
answer. It is logged distinctly so an operator can tell it from a backend error.

`tests/server/svn/llm_failure_test.rs` points the server at a dead backend and
asserts the peer reads a well-formed failure tuple carrying only a category, then
EOF.

## Known limitation: line framing

The connection is read with `read_line`. Real svn frames on tuple structure, not
newlines, and a counted string may contain a newline — any file content, any
multi-line log message. Such a message desynchronises the parser. This is why the
protocol is honest about not being usable by a real `svn checkout`: it handles
short, single-line command tuples, which covers honeypots and protocol
experiments.

## Not implemented

svndiff / delta transfer, the editor commands used by checkout and commit,
`REPORT`-style update flows, authentication beyond announcing `ANONYMOUS`
(no SASL, no credential check), locking, merge tracking, repository
administration, protocol versions other than 2, and any repository storage — the
model answers every command, nothing is stored.

## Example prompts

```json
{"type": "open_server", "port": 3690, "base_stack": "svn",
 "event_handlers": [
   {"event_pattern": "svn_greeting", "handler": {"type": "static",
     "actions": [{"type": "send_svn_greeting", "mechanisms": ["ANONYMOUS"]}]}},
   {"event_pattern": "svn_command", "handler": {"type": "static",
     "actions": [{"type": "send_svn_list", "items": [
       {"name": "trunk", "kind": "dir", "revision": 1},
       {"name": "README.txt", "kind": "file", "size": 1234, "revision": 5}]}]}}]}
```

```
listen on port 3690 via svn. Fake repository with the standard trunk/branches/tags
layout, latest revision 42. Answer get-latest-rev with send_svn_success data "42"
and get-dir with send_svn_list.
```

## Verified

With `nc` and static handlers (zero LLM calls) on 127.0.0.1:

```
-> ( success ( 2 2 ( ANONYMOUS ) ( edit-pipeline svndiff1 ) ) )
-> ( success ( 0 ( ) ( ( 5:trunk dir 0 false 1 ( ) ( ) ) ( 10:README.txt file 1234 false 5 ( ) ( ) ) ) ) )
```

Balanced, with counted strings. **Not verified against the `svn` client** — it is
not installed on the development machine, so conformance beyond the grammar is
unproven.

`tests/server/svn/` **is** declared in `tests/server/mod.rs` and runs: five mocked
e2e cases (`e2e_test.rs`, no longer `#[ignore]`d) plus a zero-LLM peer-injection
case (`peer_inject_test.rs`) that asserts `send_to_peer` writes a success tuple to
a raw socket, the counters move, and `close_connection` sends EOF.
