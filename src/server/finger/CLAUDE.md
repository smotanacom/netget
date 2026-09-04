# Finger Protocol Implementation

Finger (RFC 1288) server: the client sends one line, the server answers with free
text and closes. The model invents every user.

**State**: Experimental — **not** validated against a real client, and the reason
is structural rather than "nobody got round to it". See *Maturity* below.
**Privilege**: declares `PrivilegedPort(79)`; the preflight fires only when the
requested port is actually below 1024, so a test on port 7979 needs no
privileges. **Stack**: `ETH>IP>TCP>FINGER`.

## Protocol

RFC 1288 §2.3 gives the whole grammar:

```text
{Q1} ::= [{W}|{W}{S}{U}] {C}          -- local query
{Q2} ::= [{W}{S}][{U}]{H}{C}          -- forwarding query
{W}  ::= "/W"                         -- ask for the long format
{U}  ::= username
{H}  ::= @hostname | @hostname{H}
{C}  ::= CRLF
```

1. Client connects on TCP 79.
2. Client sends the query, CRLF-terminated. An empty line means "everyone".
3. Server writes free text and **closes**.

`FingerQuery::parse` implements exactly this and is the only parsing in the
protocol. It is lenient in two ways that cost nothing: `/W` is matched
case-insensitively, and a bare LF terminator is accepted (RFC 1288 requires CRLF
and every real client sends it, but a hand-typed `nc` session or a `printf`
without `\r` is not worth refusing). It is strict where it matters: an `@`
anywhere makes the query a forwarding query.

## One query, one answer, then close — deliberately

**This is the one place where copying WHOIS would have been wrong.** WHOIS's loop
keeps reading after a response, which is a documented non-conformance with a
visible symptom: `whois(1)` reads until EOF, so a handler answering without
`close_connection` blocks a real client forever.

Finger has the same read-until-EOF client behaviour and the trap was avoided
rather than inherited. `run_finger_session` reads **one** query line, answers,
and returns; the caller then shuts the socket down. Consequences worth knowing:

- `close_connection` is an **early exit**, not a requirement. Answering with
  `send_finger_user` alone is correct and the client is not left hanging. The
  action descriptions say so, so the model is not taught a superstition it does
  not need.
- Answering with `close_connection` *only* means "say nothing" — and the server
  turns that into `finger: no information available`, because closing in silence
  is indistinguishable from a broken server and the client is blocked on EOF
  either way. That line asserts nothing about any user.
- A second query on the same connection is never read. No real finger client
  sends one; RFC 1288 has no notion of a session.
- `tests/server/finger/e2e_test.rs` reads to **EOF** rather than doing one
  `read()`, so the close is asserted on every test rather than assumed.

## Forwarding (`user@host`) is refused, before the model

RFC 1288 §3.2.1 calls forwarding a security risk and recommends against it. Two
independent things are true here, and both matter:

1. **There is no outbound code path.** Nothing in `src/server/finger/` opens a
   socket. Whatever the model, a handler or the operator asks for, the named
   host is never contacted. This is structural, not a policy check.
2. **By default the query is refused before the LLM is consulted**, with RFC
   1288 §3.2.1's own wording — `Finger forwarding service denied.` — and a
   `decision=forward_refused` WARN naming the peer, the username and the host.
   Fixed text, one code path, no model round trip, so it costs no LLM budget and
   cannot be talked out of.

`answer_forward_queries: true` (startup parameter) changes **only** point 2: the
query reaches the model as a `finger_query` event carrying `forward_host`, and
the answer is invented locally. It logs `decision=forward_answered_locally` and a
WARN at startup, so the relaxed setting is visible in the log rather than
inferable from behaviour. It still contacts nothing.

`forward_host` is on the event schema in both modes so the *attempt* is legible
to a handler and in the TUI — surfacing it is not the same as honouring it.

## What the model sees and controls

**Event**: `finger_query`, exactly one per connection, carrying `username`
(null for the empty query), `verbose` (`/W` was sent), `forward_host` (null
unless `user@host`) and `list_all`.

**Actions**

| Action | Effect |
|---|---|
| `send_finger_user` | the conventional block: `Login:`/`Name:`, `Shell:`, `Office:`, the `On since … on … , idle …` line, `Project:` and `Plan:` |
| `send_finger_response` | free text; line endings normalised to CRLF |
| `send_finger_error` | `finger: <message>` |
| `close_connection` | answer with nothing (see above) |

No async actions: a finger server says nothing until it is asked.

`send_finger_user` takes only `login` as required. **A missing field omits its
line entirely rather than printing a placeholder** — an invented
`Never logged in.` would be a positive assertion the model never made. The
presence line is assembled from whichever of `login_time`, `tty` and `idle` were
supplied, and is omitted if none were.

### Injection hygiene

Every single-line field is stripped of ASCII control characters before it
reaches the wire, and the parsed query is stripped before it reaches the event.
In a line-oriented free-text protocol a stray CR or LF **forges a line**, and the
peer cannot tell a forged line from a real one. `plan` and `project` keep their
newlines (normalised to CRLF) because they are inherently multi-line, and they
are printed under their own heading where an extra line is not a forgery.

### Bounded read

`MAX_QUERY_BYTES` (1024) caps the accumulator. RFC 1288 sets no limit, and an
unbounded one is a memory sink for anything that connects and never sends a
newline. Over the cap the connection gets `finger: query too long` and closes.

There is **no idle timeout**: a peer that connects and sends nothing holds its
task until it disconnects or the server stops. `stop_server` reaches it — every
connection task is registered with `register_server_task`, not just the accept
loop, because aborting a task does not abort tasks it spawned.

### Dashboard injection (peer handle)

Every connection registers a peer handle (`server::peer_support`) **before** its
first read, because the server says nothing until the client speaks and a manual
`*` rule parks the query for as long as the operator takes. The write half is an
`Arc<Mutex<WriteHalf>>` shared by the session and the peer command task;
`[ message this peer ]` runs any action above through the same executor the
model's go through, and `[ disconnect this peer ]` is `close_connection`. The
handle is removed on every exit path.

### Failure behavior

An LLM failure answers the peer with a **fixed** category line —
`finger: backend at capacity, retry later` or
`finger: request could not be processed` — selected by
`crate::utils::WireFailure::classify`. These are byte literals, not a format
string: there is no placeholder an error could ever reach. The error itself goes
to the log and the TUI status stream.

## No storage, ever

This is sharper here than elsewhere in NetGet. A real finger daemon exists to
read `/etc/passwd`, `utmp` and `~/.plan`, which is precisely why finger was
turned off everywhere. **This implementation reads none of them** — no passwd
lookup, no utmp, no filesystem access of any kind. Every user is invented by the
model or by a handler. Pointing it at a real host discloses nothing about that
host.

## Maturity — why Experimental and not Beta

The bar for Beta is "works against real clients", evidenced by a test an
independent implementation passes. The real client is `finger(1)`, which is
installed on macOS/BSD and Linux. Its usage is:

```text
finger [-46gklmpsho] [user ...] [user@host ...]
```

**There is no port option**, and `user@host:port` is rejected before any
connection is attempted (`finger: 127.0.0.1:7979: nodename nor servname
provided, or not known`). It resolves the `finger` service — `/etc/services`
line `finger 79/tcp` — and always connects to TCP 79. Verified on this machine;
`man finger` documents no port flag, and the `FINGER` environment variable
carries options, of which none is a port.

So driving the real client needs a run privileged enough to bind port 79. The
e2e suite does not do that, and deliberately does **not** contain an `#[ignore]`d
root test instead: the root `CLAUDE.md` records two protocols stuck at
Experimental for exactly that, because an ignored test proves nothing and a
skip-when-missing gate is a silent pass. `Experimental` is the honest rating for
"compiles, is tested at the byte level, and no real client has ever spoken to
it".

**What would move it to Beta**: one privileged run of `finger alice@127.0.0.1`
against this server on port 79, asserting the block it prints. That is a real
test somebody has to actually run, not a test file.

## Not implemented

Forwarding/relaying (refused by design, above), `finger.conf` aliases, the
`.nofinger` opt-out, RFC 1288's suggested rate limiting, multibyte character
handling (`finger(1)` does not do it either), and any local data source. Query
and response are treated as ASCII/UTF-8 text.

## Example prompts

```json
{"type": "open_server", "port": 79, "base_stack": "finger",
 "event_handlers": [{"event_pattern": "finger_query", "handler": {"type": "static",
   "actions": [{"type": "send_finger_user", "login": "alice", "name": "Alice Smith",
     "tty": "ttys002", "idle": "5 minutes", "login_time": "Mon Sep  1 09:12",
     "office": "Room 101", "office_phone": "x1234", "shell": "/bin/sh",
     "plan": "Ship the finger server."}]}}]}
```

```
Finger server on port 7979. For any login that is asked for, invent a plausible
record. For the empty query, list two users. For user@host, we refuse anyway.
```

## Verified

By `tests/server/finger/e2e_test.rs`, which passes. Both blocks below are
`assert_eq!`ed **byte for byte** against what the socket read, so this section
cannot drift from the implementation without the test failing:

```
-> alice CRLF
Login: alice                            Name: Alice Smith
Shell: /bin/sh
Office: Room 101, x1234
On since Mon Sep  1 09:12 on ttys002, idle 5 minutes
Project:
Networking
Plan:
Ship the finger server.
<- EOF
```

```
-> alice@relay.example.invalid CRLF
Finger forwarding service denied.
<- EOF
```

(Every line above ends CRLF; the `EOF` is asserted too — the test reads to end of
stream, so a server that failed to close would hang the test rather than pass
it.) No real `finger(1)` client has spoken to this server; see *Maturity*.

`tests/server/finger/` is declared in `tests/server/mod.rs` and runs.
