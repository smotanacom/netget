# WHOIS Protocol Implementation

WHOIS (RFC 3912) server: the client sends one line, the server answers with free
text. The model decides what the registry says.

**State**: Beta — human-reviewed, verified against a real client.
**Privilege**: declares `PrivilegedPort(43)`; the preflight fires only when the
requested port is actually below 1024, so a test on port 4343 needs no
privileges. **Stack**: `ETH>IP>TCP>WHOIS`.

## Protocol

1. Client connects on TCP 43.
2. Client sends the query terminated by CRLF.
3. Server writes the response and (usually) closes.

There is nothing else to it — no versioning, no content type, no framing beyond
the line. This is why the implementation is a plain `tokio` read/write loop with
no library.

## What the model sees and controls

**Event**: `whois_query`, one per read, carrying `query` (the trimmed line).

**Actions**

| Action | Effect |
|---|---|
| `send_whois_record` | formats `Domain Name:` / `Registrar:` / `Registrant Name:` / `Admin Name:` / one `Name Server:` per entry, CRLF-terminated |
| `send_whois_response` | free-form text; CRLF appended if missing |
| `send_error` | `Error: <message>` |
| `close_connection` | closes after the response |

No async actions: WHOIS is purely reactive.

The connection loop keeps reading after a response, so a client may send several
queries; each raises its own event. Most clients send one and close.

**This is a non-conformance, but no longer an open-ended one.** RFC 3912 says the
server closes as soon as its output is finished, and `whois(1)` reads until EOF —
so a handler that answers with `send_whois_record` alone used to leave a real
client blocked forever. It now blocks for `IDLE_AFTER_REPLY_TIMEOUT` (15s) and
then gets EOF. Pairing the answer with `close_connection` is still the right
thing to do and is what both `send_*` action descriptions say;
`tests/server/whois/e2e_test.rs` proves the real client is satisfied when they
are paired. The timeout is a floor under the mistake, not a substitute.

### Injection hygiene

A WHOIS **response** is free text and `send_whois_response` is deliberately
unfiltered — multi-line output is its whole purpose. A WHOIS **record** is not
free text: it is `Key: value` lines, so every single-line field of
`send_whois_record` (and `send_error`'s `message`) is stripped of control
characters first. Without that, a `registrar` of
`"Foo\r\nRegistrant Name: Bar"` forged a field the model never asserted, and
nothing reading the output — a human, or more dangerously a script grepping for
`Registrant Name:` — could tell it from a real one.

This is the line-oriented family's signature defect. What is worth recording is
the direction it was found in: **both of this server's neighbours had added the
guard independently and WHOIS, the one they were told to copy, had not** —
`gopher`'s `sanitize_field`, `finger`'s `strip_controls`, `ident`'s
`sanitize_token`.

CR, LF and tab become a space rather than vanishing (gopher's choice): deleting
them concatenates the two sides into one word, `Good RegistrarRegistrant`, which
reads as a single value and is its own small lie. The guarantee is *no forged
line*, not that the words disappear.

### Framing and bounds

A query is a **line**, not a TCP segment. Reads accumulate to a newline under
`MAX_QUERY_BYTES` (4 KiB); past that the peer gets `% netget: query too long` and
the connection closes. Waiting for bytes is bounded too —
`FIRST_QUERY_READ_TIMEOUT` (30s) before the first query,
`IDLE_AFTER_REPLY_TIMEOUT` (15s) after one has been answered.

All three bounds replace behaviour that was reachable by anyone who could open a
socket. The loop previously raised one `whois_query` event per `read()`, so a
query split across two segments produced two events carrying two fragments, and a
peer sending a byte at a time bought **one LLM call per byte** — unmetered model
work with no authentication in front of it. `tests/server/whois/line_framing_test.rs`
pins both the reassembly and the cap.

### Dashboard injection (peer handle)

Every connection registers a peer handle (`server::peer_support`) before its first
read — a WHOIS server says nothing until asked, and a manual `*` rule can park the
query, so the operator must be able to reach the connection while it waits. The
write half is an `Arc<Mutex<WriteHalf>>` shared by the session and the peer command
task; `[ message this peer ]` executes any of the actions above through the same
executor the model's go through (all wire verbs return `ActionResult::Output`, so
no Custom-result gap), and `[ disconnect this peer ]` is `close_connection`. The
handle is removed on every exit path, and `bytes_*`/`packets_*` are updated on
every read and on every write the session itself makes; bytes written by the
generic peer task are not counted (that is in `peer_support.rs`, not here).
`tests/server/whois/peer_inject_test.rs` proves it with zero LLM calls.

### Failure behavior

**This section used to say the opposite of what the code does** — that a failure
"closes the connection with nothing written". It does not, and has not for some
time. Every failure answers.

WHOIS has no status code, so the distinction lives in a `%` comment line, which
every dialect treats as a remark and no client can mistake for a record:

| Outcome | On the wire | Log |
|---|---|---|
| backend saturated | `% netget: backend at capacity, retry later` | `decision=fail_closed_llm_error category=overloaded` |
| any other backend failure | `% netget: the query could not be answered` | `decision=fail_closed_llm_error category=unavailable` |
| model answered, nothing reached the wire | `% netget: no data was produced for this query` | `decision=model_silent` |

The two failure notices are **byte literals**, not a format string, so there is no
placeholder anything derived from the error could reach. `crate::utils::WireFailure`
classifies the error and is never rendered — see the root `CLAUDE.md` on the pass
that put netget's own retry message on strangers' terminals. `decision=` tokens
follow `src/server/radius/`: grep them to tell an outage from a model that chose
to say nothing.

## Not implemented

Referrals to another WHOIS server, WHOIS++ (RFC 1835), IDN, rate limiting,
access control, and any actual database — the model answers every query. Query
and response are treated as ASCII/UTF-8 text; there is no storage of any kind.

## Example prompts

```json
{"type": "open_server", "port": 43, "base_stack": "whois",
 "event_handlers": [{"event_pattern": "whois_query", "handler": {"type": "script",
   "language": "python",
   "code": "domain = event.get('query', 'unknown.com').strip()\nrespond([{'type': 'send_whois_record', 'domain': domain, 'registrar': 'Example Registrar Inc.', 'registrant': 'Example Organization', 'name_servers': ['ns1.example.com', 'ns2.example.com']}])"}}]}
```

```
WHOIS server on port 43 - respond with fake registration info for any domain,
registrar "Example Registrar", nameservers ns1/ns2.example.com.
```

```
listen on whois port 43. For example.com show full registration details; for
every other domain send_error "Domain not found".
```

## Verified

With a static `send_whois_record` handler (zero LLM calls) on 127.0.0.1:

```
$ printf 'example.com\n' | nc 127.0.0.1 PORT
Domain Name: example.com
Registrar: Example Registrar, Inc.
Registrant Name: Example Org
Admin Name: Admin Contact
Name Server: ns1.example.com
Name Server: ns2.example.com
```

And with the real client, which is what the `Beta` rating rests on
(`tests/server/whois/e2e_test.rs::test_whois_with_real_whois_client`):

```
$ whois -h localhost -p PORT example.com
Domain Name: example.com
Registrar: Test Registrar Inc.
Registrant Name: Test Organization
Admin Name: Test Admin
Name Server: ns1.example.com
Name Server: ns2.example.com
```

macOS's `whois(1)` **segfaults** when `-h` is given an **IP literal**
(`-h 127.0.0.1`); it does so against a plain `nc` listener too, so it is a client
bug. An earlier version of this file read that as "`-h HOST -p PORT` crashes" and
concluded the real client was unusable — it is not. `-h localhost` works, and
still resolves to loopback only.

`tests/server/whois/` is declared in `tests/server/mod.rs` and runs.
