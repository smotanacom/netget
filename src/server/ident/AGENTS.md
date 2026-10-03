# Ident Protocol Implementation (RFC 1413)

Ident answers one question: *which local account owns the TCP connection between these two
ports?* The client sends a port pair, the server answers with a userid or an error, and the
connection ends. Historically it is what an IRC daemon queries when you connect, which is
still the only place it is seen in the wild.

**State**: Experimental — see "Why not Beta" below; it is unlikely to move.
**Privilege**: declares `PrivilegedPort(113)`. The preflight fires only when the requested
port is actually below 1024, so a test on an ephemeral port needs no privileges.
**Stack**: `ETH>IP>TCP>Ident`.

## The rule that matters most: NetGet never consults the host

Ident's entire purpose is disclosing account identity. A NetGet ident server that answered
truthfully would be disclosing the *operator's own* accounts to whoever asked.

Nothing in `mod.rs` or `actions.rs` reads `/etc/passwd`, calls `getpwuid`, inspects socket
ownership, or touches any other OS state. **The model invents every userid**, and that is the
only source one can have. This is not a limitation to be fixed later — it is the reason the
protocol is safe to run at all, and it happens to coincide with the repo-wide rule that
protocols must not implement storage or consult the system.

## Protocol

1. Client connects (TCP 113 in the real world).
2. Client sends `<server-port> , <client-port>` terminated by CRLF.
3. Server answers one line and **closes**.

Replies:

```
<server-port> , <client-port> : USERID : <opsys>[,<charset>] : <userid>
<server-port> , <client-port> : ERROR  : <NO-USER|INVALID-PORT|HIDDEN-USER|UNKNOWN-ERROR>
```

There is nothing else: no versioning, no framing beyond the line, no second exchange. Hence a
plain `tokio` read/write loop and no library — there is no ident crate on crates.io to use.

### Three properties the loop owns

**1. The port pair on the wire is the pair that arrived.** RFC 1413 §3 has the client match a
reply to its query by the port pair. A reply carrying a different pair is not "slightly
wrong": the client discards it, and the result is indistinguishable from the server never
answering. Both wire actions take the pair as *required* parameters so the model is pushed to
echo it, and `enforce_port_pair` in `mod.rs` rewrites it anyway if the model got it wrong,
logging a WARN. `tests/server/ident/e2e_test.rs` has a handler that deliberately answers
`9999 , 8888` and asserts the wire still carries the queried pair.

The correction runs on the query path only. A dashboard-injected send goes through
`peer_support` and bypasses it — nothing there knows what any peer asked about.

**2. `INVALID-PORT` is decided in Rust, with no LLM call.** A port outside `1..=65535`, a
non-numeric field, a line with no comma, or a line over 1024 bytes with no newline are all
parse verdicts, not judgements. The model is never asked and no budget is spent. The refusal
still echoes the pair as the client wrote it (sanitised, 16 chars per field) because there is
nothing else the client could match on.

**3. Whitespace around the comma is tolerated.** `   113   ,   49152   ` parses. The grammar
allows it and real queries carry it; rejecting it would be a gratuitous `INVALID-PORT`.

### Bounds and task registration

The query read is capped both ways: `MAX_QUERY_BYTES` (1024, RFC 1413 §5's own limit) and
`QUERY_READ_TIMEOUT` (30s). An oversize line is *returned* rather than dropped, so the parser
rejects it and the client still gets `INVALID-PORT`; a timeout returns nothing, because a peer
that never sent a query is not waiting for a reply. Only the read is bounded — once the query
has arrived, a manual handler may park the event for its own full timeout.

**Both** the accept loop and each per-connection task are registered with
`register_server_task`. Aborting a task does not abort tasks it spawned, so before the
connection task was registered it survived `stop_server` holding its socket — and an ident
query parked on a manual handler could sit there for minutes. A comment in `mod.rs` used to
justify the omission by saying the repo had no per-connection handle store; it does, and
`finger` and `gopher` next door both use it. `register_server_task` prunes finished handles on
every call and an ident connection is one exchange long, so nothing accumulates.

### Everything the model supplies is sanitised

The reply is a single CRLF-terminated line with `:`-separated fields. A `userid` containing
CRLF would let a model append a second reply line to the stream; a `:` would shift every field
after it. `sanitize_token` strips control characters, CR, LF and `:` from `userid`, `opsys`
and `charset`, and a `,` inside `charset` too (it is appended to `opsys` after a comma, so a
second one would create a third field). `userid` is capped at RFC 1413's 512 octets.

The four error tokens are a **closed set**, validated case-insensitively against
`IDENT_ERROR_TOKENS`. Anything else is rejected with an error naming the accepted set rather
than passed through — a client parses this field, it is not free text.

## What the model sees and controls

**Event**: `ident_query`, raised once per well-formed query, carrying `server_port`,
`client_port` and `source_addr`. A malformed query never raises it.

**Actions**

| Action | Effect |
|---|---|
| `send_ident_userid` | `<pair> : USERID : <opsys>[,<charset>] : <userid>`; `opsys` defaults to `UNIX` |
| `send_ident_error` | `<pair> : ERROR : <token>`, token from the closed four |
| `close_connection` | closes — see the caveat below |

No async actions. Ident is purely reactive and the connection is over one reply later, so
there is nothing to push out of band.

`close_connection` is close to redundant: NetGet closes after every reply because RFC 1413
requires it. It exists for the dashboard's `[ disconnect this peer ]`, and answering with it
*alone* produces no output, which is then filled in as `ERROR : UNKNOWN-ERROR` — deliberately,
because leaving a client to time out is worse. Its action description says so.

## Failure behavior — answer, never silence

Ident has an error frame, so it is **not** in the deliberate-silence class the root
`CLAUDE.md` lists for ARP/OSPF/DHCP. Every failure produces `ERROR : UNKNOWN-ERROR`, which is
also the correct fail-closed answer: it asserts nothing about any account. A USERID line is a
positive claim, and a backend outage must never be able to manufacture one.

The distinction the single token erases on the wire is kept in the **log**, the way
`src/server/radius/` keeps it. The `decision=` token is stable; grep for it.

| Situation | Wire | Logged decision |
|---|---|---|
| model returns `send_ident_userid` | USERID line | `decision=model_userid` |
| model returns `send_ident_error` | that token | `decision=model_reject` |
| model returns nothing usable (no actions, only a close, or every action failed) | `ERROR : UNKNOWN-ERROR` | `decision=model_silent` |
| LLM call errors or times out | `ERROR : UNKNOWN-ERROR` | `decision=fail_closed_llm_error category=overloaded\|unavailable` |
| port pair unparseable or out of range | `ERROR : INVALID-PORT` | `decision=invalid_port (rejected in-process, no LLM call)` |

`category=` comes from `crate::utils::WireFailure::classify`. **No error text ever reaches the
peer** — `UNKNOWN-ERROR` is a fixed token from the protocol's own vocabulary, and nothing
derived from an `anyhow` chain is interpolated into a reply. That is the rule
`tests/wire_failure_test.rs` enforces.

## Why not Beta — and why that will not change easily

Beta means "works against real clients". **There is no runnable third-party ident client to
try**, and this was researched rather than assumed:

- **crates.io has no RFC 1413 client** in either direction. The `ident` namespace is entirely
  identity/identifier crates (`ident`, `unicode-ident`, `actix-identity`, …); `rfc1413` and
  `identd` return empty result sets.
- **macOS ships no client binary** — `ident`, `identd`, `oidentd`, `pidentd`, `fakeidentd` are
  all absent, and Homebrew has no `oidentd` formula. (`oidentd` is a *server* anyway.)
- **Homebrew's `libident`** is a C library with no CLI. Its API takes an already-connected
  socket and queries *its peer* on port 113.
- **PyPI has nothing working**: no stdlib module, `identlib` is dead, and the packages named
  `ident`/`pyident` are unrelated.
- **The realistic real-world client is an IRC daemon** — `ngircd` depends on `libident` and
  genuinely performs lookups. It cannot be used here.

The blocker is structural, not a matter of effort: **every ident client hardcodes destination
port 113, because RFC 1413 has no notion of a configurable server port.** None can be aimed at
an ephemeral loopback port, and 113 needs root and collides process-wide rather than being
per-test isolated.

So this is the `dhcp` situation exactly. The strongest evidence the protocol admits is a
client written from the wire format inside the test — an independent reading of the spec, not
an independent implementation — which by this repo's own bar supports Experimental and not
Beta. Do not promote it on the e2e suite alone, and do not add an `#[ignore]`d test against a
client that does not exist.

## Not implemented

No real user lookup of any kind (deliberate, see above). No `oidentd`-style per-user
`.ident` overrides, no encrypted/opaque tokens (the DES-encrypted responses some identd
implementations return), no IPv6-specific handling beyond whatever address the socket carries,
no rate limiting, no access control, no multiple queries per connection (RFC 1413 is
one-shot), and no storage.

A connected peer that sends nothing is dropped after 30 seconds (`QUERY_READ_TIMEOUT`). That
covers only the read: once the query has arrived, a manual handler may park the event for as
long as its own timeout allows, and the peer handle is registered *before* the first read so
the operator can reach the connection while it waits.

## Example prompts

```json
{"type": "open_server", "port": 113, "base_stack": "ident",
 "event_handlers": [{"event_pattern": "ident_query", "handler": {"type": "script",
   "language": "python",
   "code": "respond([{'type': 'send_ident_userid', 'server_port': event['server_port'], 'client_port': event['client_port'], 'opsys': 'UNIX', 'userid': 'nobody'}])"}}]}
```

```
Ident server on port 113 - answer every query with the userid 'nobody' on UNIX.
```

```
listen on ident port 113. For client port 6667 answer userid 'ircuser'; for everything
else answer ERROR HIDDEN-USER.
```

A **static** handler cannot echo the pair — it has no access to the event — so it is only
correct for a client whose port pair you already know. The script form above is the general
answer, and `get_startup_examples()` says so.

## Verified

Against a raw socket on 127.0.0.1 (`tests/server/ident/e2e_test.rs`, all passing):

```
$ printf '6193 , 23\r\n' | nc 127.0.0.1 PORT
6193 , 23 : USERID : UNIX : stjohns

$ printf '113 , 70000\r\n' | nc 127.0.0.1 PORT
113 , 70000 : ERROR : INVALID-PORT
```

No third-party client has validated this server, and none is available to. See above.
