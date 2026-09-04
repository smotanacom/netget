# Gopher Protocol Implementation

Gopher (RFC 1436) server: the client sends one selector line, the server answers
with a menu or a document and closes. The model authors the entire gopherspace.

**State**: Beta — human-reviewed, verified against a real client (`curl`, which
ships gopher support).
**Privilege**: declares `PrivilegedPort(70)`; the preflight fires only when the
requested port is actually below 1024, so a test on an ephemeral port needs no
privileges. **Stack**: `ETH>IP>TCP>GOPHER`.

## Protocol

1. Client connects on TCP 70.
2. Client sends `<selector>\r\n`. An empty selector means the root menu. A
   type-7 search request is `<selector>\t<query>\r\n`.
3. Server writes a menu or a document, terminated by a line containing only `.`,
   and **closes**.

That is the whole protocol. There is no version, no length, no content type and
no second request — which is why the implementation is a plain `tokio` read/write
sequence with no library.

**The reply has no length, so the close *is* the framing.** A client cannot know
the transfer is complete until it sees EOF. Measured: `curl` waits for EOF and
exits 28 (`Operation timed out`) against a server that answers and does not hang
up.

### Read-until-EOF: the deliberate choice, and why it differs from WHOIS

`src/server/whois/` keeps reading after its response, which is a documented
non-conformance with a visible symptom — a handler that answers without
`close_connection` leaves `whois(1)` blocked forever, and every action
description there has to warn about it.

**This server does not repeat that.** `run_gopher_session` reads exactly one
line, answers it, and returns; `handle_gopher_connection` then shuts the write
half down. That is what RFC 1436 §2 specifies ("the server … sends the requested
item, and then closes the connection"), and it means:

- no handler can produce a hang, because closing is not the handler's job;
- `close_connection` is still offered, but only for *hanging up without a
  reply* — its description says so rather than warning about a trap;
- the `.\r\n` terminator and the FIN are both always present, so both a client
  that looks for the dot and one that reads to EOF are satisfied.

The cost is that a peer cannot pipeline a second selector. Nothing does: Gopher
has no notion of one, and neither `curl` nor any client of that era sends one.
Bytes arriving after the first line are discarded with the socket.

## What the model sees and controls

**Event**: `gopher_request`, raised once per connection, carrying:

| Field | |
|---|---|
| `selector` | the selector asked for; empty means the root menu |
| `search_query` | **present only** for a type-7 request (a tab in the request line) |

`search_query`'s *absence* is meaningful — it separates "open the search form"
from "search for the empty string" — so the key is omitted rather than set to
`""`. The split takes the **first** tab only; a query may contain tabs of its own.

**Actions**

| Action | Effect |
|---|---|
| `send_gopher_menu` | `items` → one `<type><display>\t<selector>\t<host>\t<port>\r\n` line each, then `.\r\n` |
| `send_gopher_text` | `text` → CRLF endings, leading dots doubled, then `.\r\n` |
| `send_gopher_error` | `message` → `3<message>\t\terror.host\t1\r\n.\r\n` |
| `close_connection` | hang up without replying at all |

No async actions: a Gopher server says nothing until a selector arrives.

### Menu items are structured, never a pre-formatted blob

Each item is `{type, display, selector, host, port}`. The model never writes a
tab, a CRLF or the terminator — assembling them is this module's job, which is
the repo rule about raw bytes in action parameters applied to a tab-delimited
format.

Supported `type` values, and nothing else: `0` text file, `1` directory, `3`
error, `7` search, `9` binary, `g` GIF, `I` image, `h` HTML, `i` informational.
An unsupported type is **refused** rather than emitted: some clients silently
skip an unknown type and others render it as garbage, so the model would never
learn it had made a mistake. A refusal surfaces as an action failure an operator
can see.

Defaults fill in the tedious parts. For `i` (informational — not a link, so there
is nothing to point at) the conventional `fake` / `(NULL)` / `0` are used; for
everything else, `selector` defaults to empty and `host`/`port` to
`127.0.0.1`/`70`.

`display`, `selector` and `host` are sanitized: a tab or CR/LF inside any of them
becomes a space. Otherwise the model would be forging extra menu fields, or an
extra menu line, by accident — the structural equivalent of a header-injection
bug.

### Periodating

`send_gopher_text` doubles any leading `.` so a line that genuinely begins with a
period cannot be mistaken for the terminator. RFC 1436 has the client undo it.
`curl` does **not** — it does no Gopher-level parsing at all — so the doubled form
is what appears in curl's output, and the e2e test asserts exactly that.

One trailing newline in the supplied text is absorbed rather than becoming a
blank final line; further blank lines are the author's own and are kept.

### Failure behavior

`crate::utils::WireFailure` classifies the failure and the peer is answered with a
**type-3 item carrying the category**:
`3netget: request could not be processed\t\terror.host\t1\r\n.\r\n`. Type 3 is the
only error Gopher has, and the peer is answered rather than dropped — silence
would leave a client waiting on its own timeout.

Nothing derived from the error reaches the wire: no backend URL, no model name,
no retry text, no `anyhow` chain. That goes to the log and the TUI. The category
strings are `&'static str` for exactly this reason, and
`test_gopher_llm_failure_is_a_type_3_category_not_an_error_string` asserts both
halves — that the peer is answered, and that the answer leaks nothing.

The same type-3 item is written when the model *is* reached but produces no
output (every action failed), unless it explicitly asked to close.

### Dashboard injection (peer handle)

Every connection registers a peer handle (`server::peer_support`) **before** its
first read. A Gopher server says nothing until asked, and a manual `*` rule parks
that request for as long as the operator takes to answer — without a handle in
place first, `[ message this peer ]` would be greyed out for exactly the window in
which it is wanted. All three wire verbs return `ActionResult::Output`, so an
injected send goes through the same executor as the model's. The handle is removed
on every exit path.

### Task registration

Both the accept loop and **each per-connection task** are registered with
`AppState::register_server_task`, so `stop_server` cancels a connection parked on
a manual handler instead of leaving it running. That is one better than the
repo-wide default (per-connection tasks are usually untracked) and costs nothing
here: `register_server_task` prunes finished handles on every call, and Gopher
connections are one request long.

## Not implemented

- **Gopher+** (the `+` attribute protocol, `!`/`$` requests, `+INFO` blocks).
- **`gophers`** — Gopher over TLS. There is no RFC for it; curl supports it, this
  server does not.
- **Binary items.** Types `9`, `g` and `I` can be *listed* in a menu, but there is
  no action that serves binary content — deliberately, because that would mean
  base64 in an action parameter, which the repo rules forbid.
- **Any filesystem serving.** There is no document root, no disk access and no
  storage of any kind. The model answers every selector.
- A selector line longer than 8 KB with no line ending is refused; RFC 1436 sets
  no limit, but an unbounded read is a memory hole anyone with a socket can reach.

## Example prompts

```json
{"type": "open_server", "port": 70, "base_stack": "gopher",
 "event_handlers": [{"event_pattern": "gopher_request", "handler": {"type": "static",
   "actions": [{"type": "send_gopher_menu", "items": [
     {"type": "i", "display": "Welcome to the hole"},
     {"type": "0", "display": "About", "selector": "/about.txt", "host": "127.0.0.1", "port": 70}]}]}}]}
```

```
Gopher server on port 70. Root menu lists an About text file at /about.txt and a
Files directory at /files. Serve /about.txt as a short document. Anything else is
send_gopher_error.
```

## Verified

With the real client, which is what the `Beta` rating rests on
(`tests/server/gopher/e2e_test.rs`, both curl tests non-`#[ignore]`d and
hard-failing if curl is missing or lacks gopher support):

```
$ curl -sS gopher://127.0.0.1:PORT/ | cat -A | head -3
iWelcome to the hole^Ifake^I(NULL)^I0^M$
0About this server^I/about.txt^I127.0.0.1^I70^M$
1Files^I/files^I127.0.0.1^I70^M$

$ curl -sS gopher://127.0.0.1:PORT/0/about.txt
About this server
.. a line that starts with a period
end
.
```

Three things about `curl`'s gopher support, measured rather than assumed:

- **curl strips the item-type character from the URL path.**
  `gopher://h/1/menu` and `gopher://h/0/menu` both put `/menu` on the wire;
  `gopher://h/` sends an empty selector and `gopher://h/1/` sends `/`. The type in
  a Gopher URL says what reply the *caller* expects — the server never sees it. So
  curl cannot exercise anything type-dependent on the request side.
- **curl does no Gopher-level parsing.** The reply reaches stdout verbatim: the
  terminating `.` line is still there and the doubled leading dots are not undone.
  That is what makes the terminator and the escaping assertable through curl at all.
- **A type-3 error is still exit 0.** curl has no notion of a Gopher error, so an
  error reply must be asserted on stdout, never on the exit status.

`tests/server/gopher/` is declared in `tests/server/mod.rs` and runs.
