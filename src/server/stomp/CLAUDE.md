# STOMP Protocol Implementation

STOMP 1.2 (Simple Text Oriented Messaging Protocol) broker. A client opens a session with
`CONNECT`, then publishes with `SEND` and receives with `SUBSCRIBE`/`MESSAGE`. The model decides
whether to admit a session and what every message carries; there is no broker behind it.

**State**: Experimental. **Privilege**: `None` — the default port is 61613, which is
unprivileged, so declaring `PrivilegedPort` would be dead code (the `svn` mistake).
**Stack**: `ETH>IP>TCP>STOMP`. **Library**: none — the codec is `frame.rs`, ~300 lines.

## Why the rating is Experimental, precisely

The bar for `Beta` is a test in which an **independent implementation** completed a real
exchange. There is none here.

- **Proven.** The codec round-trips against byte literals written from the spec: escaping and
  its `CONNECT`/`CONNECTED` exemption, `content-length`-authoritative bodies containing NUL,
  NUL-terminated bodies without it, `\r\n` line endings, heart-beat EOLs, frames split across
  every possible read boundary, and the size bound. End to end, a raw socket completes
  CONNECT → SUBSCRIBE → MESSAGE → SEND → MESSAGE → DISCONNECT with receipts, and a binary body
  survives the round trip byte for byte.
- **Not proven.** No third-party STOMP client has ever spoken to this server. The e2e peer is a
  hand-written frame reader in the test file — an independent *reading* of the spec, not an
  independent implementation, which is the same class of evidence the root `CLAUDE.md` records
  for `dhcp` and `usb/serial` and explicitly does not accept for Beta.

**A dependency would fix that, and is not added here** (`Cargo.toml` was single-writer while
this was written). `async-stomp 0.6.3` is a real client: `Connector::connect()` opens the
socket, sends `CONNECT`, and **verifies the reply is `CONNECTED`**, then hands back a
`Framed` sink/stream of typed frames. Two things about it were checked against this
implementation and match:

- it hard-errors if `CONNECTED` carries no `version` header — this server always sends one;
- it hard-errors if `MESSAGE` lacks any of `destination`, `message-id`, `subscription` —
  `send_stomp_message` requires all three.

It also sends `accept-version:1.2` and no `heart-beat` header, so it needs nothing this server
does not do. Its licence is **EUPL-1.2**, which is why it wants a look at `LICENSE_ANALYSIS.md`
before landing even as a dev-dependency; `tokio-stomp 0.4.0` (MIT, unmaintained since 2023, no
rustls) is the fallback with the same handshake semantics.

There is **no** command-line STOMP client on macOS and none in Homebrew, so the `npm`/`git`
"real binary" route is not available. `stomp.py` would have to be pip-installed, which makes it
a skip-when-missing gate — evidence the root `CLAUDE.md` rejects.

## Where the work is divided

Everything mechanical is Rust and is never asked of the model:

| Done in Rust | Why |
|---|---|
| frame boundaries, NUL terminator, `content-length` | a model that gets framing wrong produces a stream no client can resynchronise from |
| header escaping (`\r` `\n` `\c` `\\`) and its `CONNECT`/`CONNECTED` exemption | same, and an unescaped `:` forges a header |
| the `receipt` handshake | a property of the frame, not of what it means |
| heart-beat negotiation (always `0,0`) | see below |
| refusing a frame that violates the spec | a stranger must not be able to provoke an LLM round trip with a malformed frame |

The model decides content: admit this `CONNECT` or refuse it, what a `MESSAGE` carries.

### No per-connection state machine, deliberately

`src/server/tcp/mod.rs` carries Idle/Processing/Accumulating because raw TCP has no frame
boundaries: a second read arriving mid-LLM-call has to be queued. STOMP *has* frame boundaries,
so this connection reads, parses out whole frames, and handles them one at a time in the same
task. Bytes arriving during an LLM call sit in the socket buffer — TCP's own backpressure — and
are read on the next pass. There is no window in which two LLM calls could run for one
connection, so there is no state to track. Copying TCP's machine here would be dead code.

### Heart-beating is negotiated off and there is no parameter for it

`CONNECTED` always carries `heart-beat:0,0`, and `send_stomp_connected` has **no** heart-beat
parameter. The server implements no heart-beat timer, so any other value would be a promise it
does not keep, and a client that believes it will hear from us every N ms tears the connection
down when it does not. Per the spec's negotiation formula, `0,0` from the server makes both
directions zero whatever the client asked for, so a compliant client stops expecting them. The
e2e test asks for `10000,10000` and asserts it gets `0,0` back.

## What the model sees and controls

### Events

| Event | Fields |
|---|---|
| `stomp_connect` | `accept_version`, `host`, `login`, `passcode`, `heart_beat` |
| `stomp_send` | `destination`, `body`, `body_encoding`, `headers` |
| `stomp_subscribe` | `destination`, `id`, `ack_mode` |
| `stomp_unsubscribe` | `id` |
| `stomp_ack` / `stomp_nack` | `id` |
| `stomp_disconnect` | `receipt` |

`passcode` is carried so the model can make an authentication decision, the same way
`ssh_auth` sees what it is deciding about. `heart_beat` is informational: the answer is always
`0,0`.

### Actions

| Action | Effect |
|---|---|
| `send_stomp_connected` | `CONNECTED` with `version` (1.2 only), optional `session`/`server`, and `heart-beat:0,0` |
| `send_stomp_message` | `MESSAGE`; requires `destination`, `subscription`, `message_id`; optional `body` + `encoding`, `content_type`, `headers` |
| `send_stomp_receipt` | `RECEIPT`. Rarely needed — receipts are automatic |
| `send_stomp_error` | `ERROR`; the connection is closed afterwards, as the spec requires |
| `close_connection` | hangs up |

No async actions: STOMP here is purely reactive — every frame the server writes answers one the
client sent.

### Body encoding — explicit, never sniffed

A STOMP body may be binary, so `body` always travels with an encoding field and the executor
**decodes** it:

- inbound (`stomp_send`): `body` is text when every byte is ASCII graphic/whitespace, otherwise
  hex; `body_encoding` says which.
- outbound (`send_stomp_message`): `encoding` is `"utf8"` (default) or `"hex"`; anything else
  is an error naming the valid values.

There is no sniffing, deliberately — `"48656c6c6f"` is simultaneously valid text and valid hex,
and only the sender knows which it means (the `send_tcp_data` bug, `d70bb5b5`). Passing the
event's `body` and `body_encoding` straight into `send_stomp_message` therefore reproduces the
exact bytes; the e2e test asserts that with a body containing NUL and 0xFF.

`content-length` supplied through the `headers` map is dropped and recomputed: a wrong one
truncates the frame or makes the peer wait for bytes that never arrive.

## Receipts, and their ordering

Any client frame carrying a `receipt` header is answered with `RECEIPT` **after** the frame has
been processed — after the handler's own output. That is what the spec says, and it is the only
order in which a handler can answer before the acknowledgement.

`CONNECT` is exempt: `CONNECTED` is its acknowledgement and a receipt on it is not defined.

If the handler's output already begins with a `RECEIPT` frame, the automatic one is suppressed,
so a model that answers with `send_stomp_receipt` does not produce a duplicate the client would
have to reconcile.

## Failure behaviour

STOMP has an `ERROR` frame, so this server is **never silent** on failure. Two distinct paths:

- **The peer got it wrong** (unknown command, a frame before `CONNECT`, a missing `destination`
  or `id`, a version this server does not speak, unrecoverable framing). Rust decides, the
  `ERROR` names the peer's own mistake — which is theirs to read — and the connection closes.
  No LLM call is made. `tests/server/stomp/e2e_test.rs::test_stomp_protocol_errors_never_reach_the_model`
  pins this: framing must not be a question the model gets asked, or a stranger can provoke an
  LLM round trip at will.
- **netget failed.** The peer gets a `crate::utils::WireFailure` **category** (`Overloaded` vs
  `Unavailable`), never the error text; the error goes to the log and the status stream, tagged
  `decision=fail_closed_llm_error class=overloaded|unavailable`.

`stomp_connect` is the one event where model silence is not a valid answer: STOMP defines
exactly two replies to `CONNECT`, and a client that gets neither blocks forever. A connect event
that produces no bytes is answered with `ERROR` and closed, logged
`decision=fail_closed_no_answer`. Everywhere else silence is legitimate — a broker that accepts
a `SEND` and says nothing is behaving normally — and logs `decision=model_no_actions`.

Log decisions in one place: `model_reject` (the model refused), `model_close` (the model hung
up), `model_no_actions`, `fail_closed_no_answer`, `fail_closed_llm_error`, `protocol_error`,
`version_refused`.

## Dashboard injection (peer handle)

Every connection registers a peer handle (`server::peer_support`) before its first read — a
STOMP server says nothing until the client sends `CONNECT`, and a manual `*` rule can park that
frame for minutes, so the operator has to be able to reach the connection while it waits. The
write half is an `Arc<Mutex<WriteHalf>>` shared by the session and the peer command task, so
`[ message this peer ]` runs any of the actions above through the same executor the model's go
through, and `[ disconnect this peer ]` is `close_connection`.

Every task this protocol spawns is registered with `AppState::register_server_task` — the accept
loop *and* each connection task. Aborting a parent does not abort what it spawned, so an
unregistered connection task keeps its socket alive after `stop_server` has released the
listener (the BGP keepalive bug).

## Not implemented

- **Heart-beating.** Negotiated `0,0`; see above.
- **Transactions.** `BEGIN`/`COMMIT`/`ABORT` are acknowledged (a receipt if asked for, a log
  line either way) and otherwise ignored. Holding a `SEND` until `COMMIT` means queueing
  messages inside the protocol, which is storage, and protocols must not implement storage. The
  `transaction` header still reaches the model in the `stomp_send` event's `headers`, so a
  handler can do whatever it likes with it.
- **Subscription bookkeeping.** The server does not remember who subscribed to what. A
  `send_stomp_message` goes to the connection whose event is being answered, and the model
  supplies the `subscription` id (the `stomp_subscribe` event gave it one). Remembering
  subscriptions would be broker state.
- **STOMP 1.0 / 1.1.** A `CONNECT` whose `accept-version` does not include `1.2` — including
  one with no `accept-version` at all, which means 1.0 — is refused with an `ERROR` naming
  `version:1.2`, as the spec asks. Answering a dialect this server does not implement would be
  the same class of lie as advertising heart-beats.
- **TLS.** Plain TCP only.

## Example prompts

```
STOMP broker on port 61613 - accept every CONNECT, and when a client subscribes to a
destination send it one MESSAGE welcoming it to that destination.
```

```json
{"type": "open_server", "port": 61613, "base_stack": "stomp",
 "event_handlers": [{"event_pattern": "stomp_connect", "handler": {"type": "static",
   "actions": [{"type": "send_stomp_connected", "session": "session-1", "server": "netget/stomp"}]}}]}
```

```json
{"type": "open_server", "port": 61613, "base_stack": "stomp",
 "event_handlers": [{"event_pattern": "stomp_subscribe", "handler": {"type": "script",
   "language": "python",
   "code": "respond([{'type': 'send_stomp_message', 'destination': event['destination'], 'subscription': event['id'], 'message_id': 'msg-1', 'body': 'welcome', 'encoding': 'utf8'}])"}}]}
```

## Verified

`tests/server/stomp/` is declared in `tests/server/mod.rs` and runs. 21 tests, 8 LLM calls:

```bash
./cargo-isolated.sh test --no-default-features --features stomp \
    --test server -- --test-threads=100 stomp
```
