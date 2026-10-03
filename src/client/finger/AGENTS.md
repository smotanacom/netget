# Finger (RFC 1288) client

Connects to a finger server, sends **one** query line, reads the free-text answer **to EOF**,
and hands that text to the model. `Experimental`.

Two files: `actions.rs` (vocabulary, events, query grammar, validation) and `mod.rs` (connection
loop, forwarding policy, follow-up chain, command channel). No dependency was added — the query
line is three `push_str` calls.

## The shape of the protocol, and what follows from it

RFC 1288 §2.3 is the whole client side:

```text
{Q1} ::= [{W}|{W}{S}{U}] {C}          -- local query
{Q2} ::= [{W}{S}][{U}]{H}{C}          -- forwarding query
{W}  ::= "/W"                         -- long format
{U}  ::= username
{H}  ::= @hostname
{C}  ::= CRLF
```

Three consequences drive the implementation:

- **The server closes when it is done, and there is no length anywhere.** A finger client is
  therefore *defined* by reading to EOF, not by one `read()`. `read_response_inner` loops until
  `Ok(0)`. A peer that never closes would park the task forever, which is why
  `response_timeout_secs` exists.
- **One query per connection.** A second query on the same socket is ignored by every real
  server. So a follow-up the model asks for opens a **new** connection — see below.
- **The answer has no format at all.** RFC 1288 specifies none. Nothing here parses it.

## Do not over-parse the response — this is a design rule, not a preference

The response goes to the model as text. There is no `Login:`/`Name:`/`Directory:` column reader
and there must not be one: BSD `fingerd`, GNU `fingerd`, `efingerd`, the modern
finger-as-a-microblog servers and every `.plan`-only daemon print different things, and a
heuristic that works against one is wrong against the next. Interpreting prose is what the model
is for.

The one concession is `best_effort` on the response event, and every part of it is labelled:

- it scrapes `logins` from lines *beginning* `Login:` and counts lines, and nothing else;
- its own `note` field says `GUESS ONLY` and tells the model to read `response`;
- `response` — the raw text — is always present alongside it and is documented as authoritative.

If you extend it, keep those three properties. A best-effort field that quietly looks
authoritative is worse than no field.

## Forwarding is off by default, and cannot happen implicitly

RFC 1288 §3.2.1 names `user@host` forwarding a security risk. The server half of NetGet refuses
to *be* the relay; this half refuses to *ask* for one. Three separate things enforce it, because
one of them was not enough:

1. **`allow_forwarding`** (startup parameter, default `false`). While false, a
   `send_finger_query` carrying `forward_host` is refused **before anything is written** and
   logged as `decision=forward_refused`.
2. **`username` may not contain `@`.** `FingerQuerySpec::from_action` rejects it by name and
   tells the model to use `forward_host`. Without this, `{"username": "alice@relay"}` would be a
   forwarding query that never went past the gate — the gate would be decoration.
3. **`username` may not contain whitespace or start with `/`.** The query is one line; either
   would forge an extra field, `/W` included.

The gate lives in `mod.rs::apply_action`, not in `execute_action`, and the split is deliberate:
the executor decides what a *legal* query is (pure, testable, no config), the connection loop
decides what this client is *allowed* to send (that is where the startup parameter is). Both the
LLM path and the dashboard's injected commands go through the same `apply_action`, so there is
exactly one place that can put a query on the wire.

## Follow-ups: a new connection, a real event, and a bound

The model may answer `finger_response_received` with another `send_finger_query` — the natural
chase (list everyone, then finger the interesting name). Because RFC 1288 is one query per
connection, `run_query` opens a fresh one, and the answer raises `finger_response_received`
**again**. That is genuinely self-referential, which in this repo has run away before, so:

- the recursive call is `Box::pin`ned (an `async fn` awaiting itself has an infinitely-sized
  future, E0391) and the boxed type is `Send`, because it is awaited inside a `tokio::spawn`;
- `MAX_FOLLOWUP_DEPTH` (4) bounds the chain;
- **one query per turn** bounds the breadth — k queries per turn at depth d is k^d connections.
  Extra ones are refused in the log rather than dropped in silence.

The WHOIS client next door made the opposite trade: its follow-up raises no event, which keeps
the future non-recursive but makes the chain exactly one step deep. Depth-bounded recursion is
the better answer and is what the root `CLAUDE.md` prescribes.

## The model's answer is executed, never counted

Both LLM call sites bind `ClientLlmResult { actions, memory_updates }` and run every action
through `protocol.execute_action` and then `apply_action`. This is the single most common client
defect in this repo — `let _ =`, a bare `debug!` of `.len()`, `Ok(_) =>`, or a comment explaining
why the answer is not needed are all the same bug — and
`tests/client_event_wiring_test.rs` is the ratchet.

## Startup parameters (all three are read; none is decoration)

| Parameter | Default | Read in |
|---|---|---|
| `port` | 79 | `Client::connect` → `resolve_target`, used **only** when `remote_addr` carries no port |
| `allow_forwarding` | `false` | `apply_action` and the follow-up gate |
| `response_timeout_secs` | 30 (`0` = forever) | `read_response_inner` |

`port` exists for a specific reason worth keeping: the real `finger(1)` has **no port option**
and is locked to TCP 79 (it resolves the `finger` service; `user@host:port` is rejected as a
hostname). A test server on a high port is therefore reachable only through NetGet. An explicit
port in `remote_addr` always wins; `resolve_target` handles bare and bracketed IPv6, because
`::1` and `[::1]` both contain colons and a "does it contain `:`" test reads the first as
already having a port.

## Events

| Event | When | Notes |
|---|---|---|
| `finger_connected` | TCP is up, nothing sent | carries `allow_forwarding` so the model knows what will be refused before it asks |
| `finger_response_received` | the server answered and closed | `response` (raw), `query`, `username`, `verbose`, `forward_host`, `bytes`, `eof`, `truncated`, `best_effort` |

Both are emitted (`event_emit_sites_test` and `client_event_wiring_test` enforce it). `eof` is
worth reading: `false` means the read stopped before the server closed — the answer may be
partial — and `truncated` means `MAX_RESPONSE_BYTES` (64 KiB) was reached and the rest discarded.
A finger answer is a few hundred bytes; the cap is there because RFC 1288 has no length field and
a hostile server can stream forever.

## Control characters are stripped in both directions

Outbound, every value that reaches the wire goes through `strip_controls`: in a line-oriented
protocol a stray CR or LF **forges a line**, which is the same reason the server does it.
Inbound, `sanitize_response` normalises line endings to LF, keeps `\n` and `\t`, and drops every
other control including ESC — a hostile finger server would otherwise inject terminal escapes
into the operator's dashboard. No raw bytes and no base64 appear in any action parameter or event
field.

## Maturity: `Experimental`, and exactly what would earn Beta

Rated `Experimental` on the evidence, not on the code quality. The round-trip test's peer is
**NetGet's own Finger server**, which is *same-project evidence*: it shows the two halves agree,
not that either matches RFC 1288. That is the circular-evidence class the root `CLAUDE.md` names
(`webrtc_signaling`, and `websocket` before it hand-wrote a raw RFC 6455 client).

**Beta needs one run against a real finger daemon on loopback.** Unlike the server's
mirror-image problem this is *achievable in principle*: a daemon can be told which port to bind —
`in.fingerd` is inetd-driven, so any socket-activating wrapper serves it on a high port — even
though `finger(1)` cannot be told which port to dial. The asymmetry is real and it favours the
client.

It is not achievable *here* today, and this was checked rather than assumed (September 2026,
macOS):

- `/usr/bin/finger` is present — Apple ships the **client** only.
- No `fingerd` / `in.fingerd` / `efingerd` / `cfingerd` anywhere on the system; macOS has not
  shipped a finger LaunchDaemon for years.
- Homebrew has **no** formula for one: `bsd-finger`, `fingerd` and `netkit` all miss.
- No `socat`, `xinetd` or `inetd` to host an inetd-style daemon on a high port.
- `go` and `cargo` are installed, so one could be built from source (netkit's `in.fingerd`, or a
  Go implementation) — but a from-source daemon compiled for the occasion is a weaker independent
  witness than a packaged one, and it would still need a socket-activating wrapper.

Do not promote on the existing test. Do not add a `#[ignore]`d root test either — an ignored test
proves nothing, and a skip-when-missing gate is a silent pass (see `kubernetes`, `oci_registry`,
`maven`, `websocket`). If a daemon becomes available, copy `npm`'s shape: **fail** when the
binary is absent.
