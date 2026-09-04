# NATS Protocol Implementation

NATS client protocol (TCP 4222): the server greets with `INFO`, the client sends
`CONNECT`, and then both sides exchange CRLF-terminated control lines.
**Nothing is routed automatically — the model decides what every subscriber
receives.** That is the whole point of the protocol here: a real broker's value
is its routing table, and here the routing table is a language model.

**State**: Beta — human-reviewed, and verified against the official `async-nats`
client in a test that is not `#[ignore]`d and cannot skip.
**Privilege**: `None`. The default port 4222 is unprivileged, so declaring
`PrivilegedPort(4222)` would be dead code (the preflight only fires below 1024).
**Stack**: `ETH>IP>TCP>NATS`. **Library**: none — the codec is hand-written on
`tokio`.

## Protocol

1. Server accepts and **immediately** writes `INFO {json}\r\n`. A client will not
   speak until it has this, and it deserialises the document strictly enough that
   a missing or wrongly-typed field fails the connect.
2. Client sends `CONNECT {json}\r\n`, usually followed at once by `PING\r\n`.
3. Then, in any order: `PUB`/`HPUB`, `SUB`, `UNSUB`, `PING`, `PONG`.
4. Server may write `MSG`, `HMSG`, `INFO`, `+OK`, `-ERR`, `PING`, `PONG`.

`PUB <subject> [reply-to] <#bytes>\r\n<payload>\r\n` — the payload is
byte-counted, not delimited, so it may contain anything including CRLF.
`HPUB <subject> [reply-to] <#header bytes> <#total bytes>` prefixes the payload
with a `NATS/1.0\r\n…\r\n\r\n` header block.

### What is answered without the model, and why

`PING` → `PONG`, and the per-command `+OK` a client that sent
`"verbose": true` expects, are written by the reader task in Rust.

This is not an optimisation. `async-nats` writes `CONNECT` and `PING` in one
flush and blocks until it reads a reply, so routing the keepalive through the LLM
would put connect itself behind a model round-trip — and behind a *manual*
handler, behind a human. A dashboard-created server defaults to a `*` → manual
rule, so that is the normal case, not the exotic one.

A malformed frame is also answered in Rust: `-ERR '<standard NATS text>'` and a
hang-up. Once the byte stream is no longer aligned to a frame boundary there is
nothing to ask the model about. The texts are the real server's
(`Unknown Protocol Operation`, `Maximum Payload Violation`, …) because clients
match on some of them.

## Two tasks per connection

- **Reader** — frames the stream (`parse_frame`), answers the non-decisions
  above, and forwards everything else over a **bounded** channel.
- **Dispatcher** — owns the subscription table and makes one LLM call at a time,
  in arrival order.

This is the `Idle → Processing → Accumulating` machine of `src/server/tcp/mod.rs`
in a different shape: the dispatcher being busy *is* `Processing`, the channel
*is* `queued_data`, and there is no `Accumulating` state because NATS frames are
self-delimiting — there is never a partial message to accumulate. `wait_for_more`
would therefore mean nothing and is not offered.

Because the subscription table is owned by one task, no lock is held across the
LLM call. The write half is an `Arc<Mutex<WriteHalf>>` shared with the reader and
the peer-injection task; the stream is `tokio::io::split`, never cloned.

Both tasks are registered with `AppState::register_server_task`, not only the
accept loop: aborting a task does not abort tasks it spawned, so a connection
whose reader was registered nowhere would keep its socket alive past
`stop_server`.

## What the model sees and controls

**Events**

| Event | Carries |
|---|---|
| `nats_connect` | `options` (the CONNECT document verbatim), `client_name`, `lang`, `verbose` |
| `nats_publish` | `subject`, `reply_to`, `payload`, `payload_encoding`, `headers`, `matching_subscriptions` |
| `nats_subscribe` | `subject` (may contain `*` / `>`), `queue_group`, `sid` |
| `nats_unsubscribe` | `sid`, `subject`, `max_msgs` |

**Actions**

| Action | Frame |
|---|---|
| `send_nats_message` | `MSG`, or `HMSG` when `headers` is given |
| `send_nats_info` | another `INFO` |
| `send_ok` | `+OK` |
| `send_err` | `-ERR '<message>'` |
| `send_ping` | `PING` |
| `close_connection` | hang up |

No async actions: every frame this server writes answers something the peer said
on that same connection.

### `matching_subscriptions` is the mechanism that makes this usable

A model cannot deliver a message without knowing the client's subscription id,
and the client chooses those ids. Each `nats_publish` event therefore carries the
subscriptions **on that connection** whose subject filter matches, with wildcards
resolved (`subject_matches`: `*` is one token, `>` is one or more and must be
last). The list is a **hint, never an enforcement** — the model may deliver to
any sid, several, or none. Nothing is delivered because the table says so.

### Payload encoding

Inbound `payload` is the text itself when every byte is printable, and hex
otherwise; `payload_encoding` says which. Outbound `send_nats_message` takes the
same pair (`payload` + `encoding`, `"utf8"` default, `"hex"`), decoded
explicitly. There is no sniffing: `"48656c6c6f"` is simultaneously valid text and
valid hex and only the sender knows which it means — the bug
`send_tcp_data` had. Echo an event's `payload` **and** its `payload_encoding` and
the exact received bytes go back out.

### Injection guards

`subject`, `sid` and `reply_to` are rejected if they contain whitespace or a
control character, and `-ERR` text has quotes and control characters replaced.
A NATS control line is space-delimited and CRLF-terminated, so an unchecked token
would forge a second frame — a message delivered on a subject nobody asked for.
Rejecting is louder than stripping, which is why it rejects.

### Failure behaviour

An LLM failure is answered with `-ERR '<category>'` from
`crate::utils::WireFailure` (`netget: backend at capacity, retry later` vs
`netget: request could not be processed`) and then a hang-up, because NATS
clients treat `-ERR` as fatal and there is nothing further this server can say on
a connection it cannot answer for. **The peer gets a category; the log gets the
error** (`decision=fail_closed_llm_error class=overloaded|unavailable error=…`).
Nothing derived from the error reaches the wire.

The model answering with no actions is a *legitimate* answer — most publishes to
a subject nobody subscribed to produce no frame at all — and is logged at debug
as `decision=model_no_actions`, distinct from the failure above.

## Not implemented

Everything a real broker does apart from framing:

- **No routing.** A `PUB` is never delivered to anyone unless the model says so,
  including to subscriptions on the same connection.
- **No cross-connection delivery.** An action writes to the connection whose
  event triggered it (`ActionResult::Output` has no other destination), so a
  publish on connection A cannot reach a subscriber on connection B. The
  dashboard's `[ message this peer ]` is the only way to write to another
  connection, and a human drives it.
- **No queue-group semantics.** Groups are recorded and reported; nothing
  load-balances across them.
- **`UNSUB <sid> <max>` does not count deliveries.** A plain `UNSUB` drops the
  subscription; one with a `max` leaves it targetable and reports the threshold
  to the model, which must honour it. Under-delivering and over-delivering are
  both wrong, and this picks the one the model can correct.
- **No JetStream, no authentication, no TLS, no clustering, no `connect_urls`.**
  `INFO` advertises `auth_required: false`, `tls_required: false`,
  `jetstream: false` and an empty cluster, which is honest.
- **The advertised version is `2.10.0`.** Clients gate optional client-protocol
  features on it, and `headers`/`HPUB` need a version that has them. It is a
  claim about the *client protocol*, not feature parity — see the list above.
- **`send_nats_info` reports host `0.0.0.0` port `0`.** `execute_action` has no
  connection context, so it cannot know the real ones; a client uses
  `connect_urls` rather than these fields for reconnection. The greeting written
  on accept has the real values.
- **No storage.** No subject retention, no message log, no persistence of any
  kind. The subscription table is per-connection and dies with it.

### Known limitation: backpressure stops the keepalive

The reader→dispatcher channel holds 256 frames. If the model (or a human on a
manual handler) is slower than the publisher for long enough to fill it, the
reader blocks on `send` and stops reading — which also stops it answering `PING`,
and the client will eventually declare the connection stale. This is deliberate:
the alternative is an unbounded queue that grows until the process dies. It has
not been observed in practice, because a client that far ahead of the broker is
already in trouble.

## Example prompts

```
NATS server on port 4222. When a client publishes, deliver a message to every
subscription in matching_subscriptions whose payload summarises what was
published, and set reply_to if the publish had one.
```

```json
{"type": "open_server", "port": 4222, "base_stack": "nats",
 "event_handlers": [{"event_pattern": "nats_publish", "handler": {"type": "script",
   "language": "python",
   "code": "import json,sys\nd=json.load(sys.stdin)\ne=d['event']\nprint(json.dumps({'actions':[{'type':'send_nats_message','subject':e['subject'],'sid':s['sid'],'payload':e['payload'],'encoding':e['payload_encoding']} for s in e.get('matching_subscriptions',[])]}))"}}]}
```

The script above is an echoing broker with **zero** LLM calls, and is the right
default for anything deterministic.

## Verified

`tests/server/nats/e2e_test.rs::test_nats_delivers_model_authored_message_to_async_nats`
is what the `Beta` rating rests on. The official `async-nats` 0.50 client:

- completes `INFO` → `CONNECT`+`PING` → `PONG`,
- subscribes to `greetings` and publishes to it,
- receives a `MSG` whose payload the model authored **and** which quotes the
  payload the client sent (`"the model saw: hi"`), and
- receives an `HMSG` whose reply subject and `Nats-Msg-Id` header the client
  parses back out.

All of that is asserted from inside the client's own `Message`, so nothing passes
unless the framing, the byte counts and the header block are right. The client is
a dev-dependency, so the test cannot silently skip the way a `SKIP: kubectl is
not installed` gate can.

By hand, with `nc`:

```
$ nc 127.0.0.1 4222
INFO {"server_id":"NETGET-netget-nats","server_name":"netget-nats",...}
CONNECT {"verbose":true}
+OK
PING
PONG
BOGUS
-ERR 'Unknown Protocol Operation'
```

(The bare-LF tolerance in `parse_frame` exists for exactly this: `nc` sends LF,
not CRLF.)
