# WebSocket (RFC 6455) Server

**Status**: Beta (see [Validation](#validation) for exactly what was checked)

Beta rests on **websocat 1.14.1**, which links `websocket-0.27.1` / `websocket-base-0.26.5`
(rust-websocket) and **not** `tungstenite` — read out of the installed binary's embedded crate
paths. That matters: this server frames with `tokio-tungstenite`, so a tungstenite-based peer
would be the circular-evidence case, and rust-websocket is a separate RFC 6455 implementation.
`test_websocket_with_websocat` is not `#[ignore]`d and **fails** rather than skips when the
binary is absent; it printed `skipping: websocat is not installed` and returned `Ok(())` until
September 2026, which is a silent pass and is what held this at Experimental.

Still unproven, and what a human should check before this goes further: `wss://`,
permessage-deflate, and any browser.
**Spec**: RFC 6455. RFC 7692 (permessage-deflate) is **not** implemented.
**Feature**: `websocket` · **Privilege**: `None` · **System libraries**: none

## Why this protocol is a good fit for NetGet

Almost every other protocol here is request/response: a peer asks, the model answers, the
exchange ends. WebSocket is a long-lived bidirectional session, so the model gets two things it
does not get elsewhere:

1. **It can speak unprompted.** `websocket_connection_opened` fires before the client has said
   anything, and `push_websocket_text` / `push_websocket_binary` address any open connection by
   id (or `"*"` for a broadcast) from a user command or a scheduled task. A ticker that emits a
   price every two seconds is a scheduled task plus one action.
2. **The handshake is a real decision.** `Sec-WebSocket-Protocol` negotiation is a genuine
   choice — the client offers a preference list, the server picks at most one, and RFC 6455
   §4.2.2 forbids naming one that was not offered.

## Library choice

`tokio-tungstenite` **0.21**, which was already an optional dependency in the tree (pulled in by
the `webrtc` feature). Deliberately **not bumped to 0.30**: the version is shared with
`webrtc_signaling` and has to agree with whatever the `webrtc` crate itself pins, so bumping it
is a `webrtc`-wide decision and not this protocol's to make. 0.21 is fully RFC 6455 for
everything below §5, which is all that is needed here.

`fastwebsockets` was the alternative. It was rejected for one reason: adding a second WebSocket
implementation to a tree that already contains one, for a protocol the existing one covers, is
a dependency the project would then carry forever.

### The handshake is hand-written; the framing is not

`accept_hdr_async` takes a **synchronous** callback to shape the 101 response. Choosing a
subprotocol needs a model call, which is `async`. So `src/server/websocket/mod.rs` does §4.2 of
the RFC itself:

- `parse_request_head` — request line + headers, hand-parsed (obsolete line folding rejected)
- `validate_upgrade` — method/version/`Upgrade`/`Connection`/`Sec-WebSocket-Version: 13`, and
  that `Sec-WebSocket-Key` really is a base64-encoded 16-byte nonce
- `build_accept_response` — `Sec-WebSocket-Accept` via `tungstenite::handshake::derive_accept_key`
  (`base64(SHA-1(key + 258EAFA5-E914-47DA-95CA-C5AB0DC85B11))`), so there is exactly one
  implementation of the GUID concatenation in the tree
- the upgraded socket then goes to `WebSocketStream::from_partially_read(.., Role::Server, ..)`

`from_partially_read` rather than `from_raw_socket` on purpose: a pipelined client can put frames
in the same TCP segment as the request head, and those bytes would otherwise be dropped.

Everything from §5 onwards is the library's: frame parsing, the mandatory client-to-server mask
check (`accept_unmasked_frames` stays `false`, so an unmasked client frame fails the connection
per §5.1), continuation-frame reassembly, automatic pongs, and the closing handshake.

## Events

Every event below is emitted, and every one declares actions. Verify both directions with:

```bash
grep -c "with_actions\|with_no_actions" src/server/websocket/actions.rs
grep -n "_EVENT," src/server/websocket/mod.rs   # the emit side
```

| Event | When | Actions offered |
|---|---|---|
| `websocket_handshake` | every well-formed upgrade request, **before** any 101 | `accept_websocket`, `reject_websocket` |
| `websocket_connection_opened` | immediately after the 101 is written | send text/binary/ping, close |
| `websocket_text_message` | a reassembled text message | send text/binary/ping, close, `wait_for_websocket_data` |
| `websocket_binary_message` | a reassembled binary message | same |
| `websocket_ping` | a ping control frame (the pong is already on its way) | send text/binary/ping, close |
| `websocket_close` | the client started the closing handshake | send text, close |

**Pong frames are deliberately not an event.** A keepalive pong would otherwise cost a model
call per heartbeat. They are logged at DEBUG, and they count as activity for the idle bound
below — that is what they are for.

**Every connection costs two model calls before the first message** (`websocket_handshake`, then
`websocket_connection_opened`). For a deterministic endpoint use script or static handlers, which
cost none — that is how `tests/server/websocket/e2e_test.rs` runs a whole conversation on a
single `open_server` call.

## Data format — symmetric in both directions

Text frames carry a plain `text` field; there is nothing to encode, a text frame is UTF-8 by
definition.

Binary frames use the `data` + `encoding` pair, and **the two directions are inverses**:

| Direction | Field values |
|---|---|
| inbound (`websocket_binary_message`) | `encoding: "utf8"` and `data` as literal text when every byte is printable ASCII; otherwise `encoding: "base64"` and `data` base64-encoded |
| outbound (`send_websocket_binary`, `push_websocket_binary`) | `"utf8"` (default) sends the characters of `data` unchanged; `"base64"` and `"hex"` decode it |

So echoing a binary frame back unchanged is passing the event's `data` **and** its `encoding`
straight through, and the bytes on the wire are identical. This is the `send_tcp_data` bug
(`d70bb5b5`) in a protocol that would have shipped with it: there, inbound was encoded and
outbound was not, so an echo server could not echo. `test_binary_payload_encoding_is_symmetric`
and the byte-for-byte assertion in `test_websocket_wire_protocol_against_raw_client` exist
specifically to fail if that ever drifts.

There is **no auto-detection**: `"48656c6c6f"` is simultaneously valid text, valid hex and
nearly valid base64, and only the sender knows which it means.

## Fail-closed handshake

A `websocket_handshake` answered with neither `accept_websocket` nor `reject_websocket` is
**refused with HTTP 503**, and the refusal is logged at ERROR with wording distinct from an
explicit rejection. An LLM outage, a handler that returns only `show_message`, and a model that
means to deny all end at the same place, and none of them can be mistaken for consent. This is
the OAuth2 post-mortem in CLAUDE.md applied ahead of time rather than after.

A request that is not a WebSocket upgrade at all never reaches the model — there is no decision
in "this is not a handshake". It gets 400/405/426/505 directly, and a 426 carries
`Sec-WebSocket-Version: 13` as §4.4 requires.

On a **handler failure mid-session** the connection is closed with code **1011** (internal
error), or **1013** (try again later) when the failure is a saturated backend, rather than being
reset to Idle in silence — so the peer is not left waiting for a reply that is never coming, and
a client backs off rather than recording a permanent fault. The close *reason* is the fixed
string `handler failed` in both cases: a close reason is peer-visible, so the backend's error
text belongs in the log line beside it and nowhere else.

## Failure behaviour

Every LLM call site emits one `decision=` line. The important one is `websocket_text_message` /
`websocket_binary_message` / `websocket_ping`: a handler that answers with nothing sends no
frame at all and the connection simply goes quiet, which used to be logged nowhere — so a model
that deliberately said nothing and a rule that never matched were the same silence.

| Outcome | On the wire | Log |
|---|---|---|
| `websocket_handshake` → `accept_websocket` | 101, with the agreed subprotocol | INFO `decision=model_answer` |
| `websocket_handshake` → `reject_websocket` | the handler's own status (default 403) | INFO `decision=model_reject` |
| `websocket_handshake` → neither, no action at all | **503**, fixed reason | ERROR `decision=fail_closed_no_action` |
| `websocket_handshake` → neither, but actions ran | **503**, fixed reason | ERROR `decision=fail_closed_bad_action` |
| `websocket_handshake` → backend failed | **503**, fixed reason | ERROR `decision=fail_closed_llm_error` / `..._overloaded` |
| message → frames sent, or `wait_for_websocket_data` | those frames, or the message is held | INFO `decision=model_answer` |
| message → `close_websocket` | close frame with the model's code and reason | INFO `decision=model_reject` |
| message → no action | **nothing**; the connection stays open | WARN `decision=model_silent` |
| message → every action refused by the executor | **nothing**; the connection stays open | ERROR `decision=fail_closed_bad_action` |
| message → backend failed | close **1011** / **1013**, reason `handler failed` | ERROR `decision=fail_closed_llm_error` / `..._overloaded` |
| `websocket_connection_opened` → backend failed | nothing; the connection stays open | WARN `decision=llm_error_notice_only` |
| `websocket_close` → backend failed | nothing; the peer has already closed | WARN `decision=llm_error_notice_only` |

`llm_error_notice_only` is this protocol's own token and is deliberately **not** `fail_closed_*`.
Both events it covers fire when nothing is pending: the upgrade was already decided by
`websocket_handshake`, and `websocket_close` fires because the peer has gone. Tagging them
`fail_closed_` would put lines that refused nothing in front of every
`grep decision=fail_closed`.

`tests/server/websocket/e2e_test.rs::test_websocket_handshake_backend_failure_is_tagged_fail_closed`
asserts both halves of the handshake row: the 503 on the wire, and
`decision=fail_closed_llm_error` in the log with no `decision=model_reject` anywhere.

## Connection state machine

Per connection: **Idle → Processing → Accumulating**, in `ConnData`.

- messages arriving while the model is thinking go on `queued` (no concurrent model call on one
  connection, no lost message)
- `wait_for_websocket_data` moves to `Accumulating` and parks the message in `pending`; the next
  message of the same kind is joined onto it (text concatenated, binary appended) and the pair is
  delivered as one event. Different kinds are processed in arrival order instead.

`tokio::io::split` is not used directly: the socket becomes a `WebSocketStream`, which is split
with `futures::StreamExt::split()` into a `SplitSink` and a `SplitStream` — the same "one owner
per direction, never clone" shape one level up. A single **writer task** owns the sink and every
producer (message handlers, async push actions, scheduled tasks) sends `WsOut` down an
`mpsc::UnboundedSender`, so no lock is ever held across an `.await` that performs I/O.

The auto-pong is flushed by the *next* read, which the frame loop performs immediately, so a
ping is answered even when the handler produces no frame of its own.

## Startup parameters

Every one is declared and read; nothing is declared and unused.

| Parameter | Effect |
|---|---|
| `path` | only this exact path is upgraded; anything else gets 404 without a model call |
| `max_message_size` | reassembled-message ceiling, default 1 MiB, clamped to 64 MiB |
| `max_frame_size` | single-frame ceiling, same default and clamp |
| `idle_timeout_secs` | how long an upgraded connection may send no frame at all, default 600 |

## Connection bounds

| Bound | Default | Mechanism |
|---|---|---|
| Request head | 15s (`HANDSHAKE_TIMEOUT_SECS`) | `timeout` around reading the HTTP head, before the upgrade |
| Upgraded, no frame from the peer | 600s (`idle_timeout_secs`) | `accept_bounded::watch_idle_with_probe` over a `ConnectionActivity`, the probe a Ping, raced against the frame loop |
| Connections | 256 | `accept_bounded`; the peer over the cap reads `503` + `Retry-After` |

**The upgraded bound is on liveness, not on conversation.** A WebSocket client is entitled to
hold a session open with nothing to say, so at half the bound the server sends a Ping (payload
`netget-keepalive`), and every RFC 6455 endpoint answers a Ping with a Pong by itself (§5.5.2)
— browsers, tungstenite, websocat, NetGet's own client — with no application code. Any inbound
frame counts: text, binary, Ping, Pong, Close. What reaches the bound is a peer that has stopped
reading or vanished without a FIN. It is closed with **1001** ("going away", §7.4.1) and the log
says `decision=idle_timeout`.

**Why 600 and not 300: NetGet's own client.** `src/client/websocket/mod.rs` runs each inbound
message's model turn inside its read loop, so while a turn is parked for a human (a `manual`
rule's 300-second window) it reads nothing and its Pong sits in tungstenite's queue. At twice
that window, a Ping sent at the worst moment is still answered — late — inside the bound. The
client-side fix is to keep reading while a turn is parked; until then this number is what keeps
the operator's own peer from being dropped mid-answer.

**Busy is not idle.** Handlers run on their own tasks while the frame loop keeps reading, so each
holds `ConnectionActivity` busy for the whole of its answer; a message waiting on the model or
parked for a human by a `manual` rule is neither pinged nor closed, however long it takes.

`tests/server/websocket/connection_bounds_test.rs` drives all of it from the wire.

## Connection directory (not storage)

`WS_CONNECTIONS` maps connection id → live `mpsc` sender, remote address, path and negotiated
subprotocol. It exists so `push_websocket_text` can name a recipient, and an entry lives exactly
as long as its socket. No message log, no rooms, no subscriptions — the model tracks all of that
in its memory, per the no-storage rule.

## Keyword and stack-name notes

`stack_name()` is `ETH>IP>TCP>HTTP>WebSocket`, **not** `ETH>IP>TCP>WebSocket` — WebRTC Signaling
already claims that string, and stack names are registered as parser keywords, so a duplicate
would make `parse_from_str` resolve by HashMap iteration order. The HTTP hop is also more
accurate.

Claiming the keyword `websocket` would have shadowed WebRTC Signaling's longer
`websocket signaling` in the same arbitrary-order loop, so `server_registry.rs` gained a
priority check for `"WebRTC Signaling"` ahead of the generic keyword loop, in the same shape as
the existing mDNS-before-DNS and Proxy-before-HTTP entries. No bare `ws` keyword: it would match
`aws`, `wsdl` and more.

## Validation

Checked against **three peers, two of them not this repository's code**:

1. **A hand-written raw TCP client** (`tests/server/websocket/e2e_test.rs`) that composes the
   upgrade itself, recomputes `Sec-WebSocket-Accept` from the RFC algorithm with `sha1` +
   `base64` directly, masks its own frames, and decodes the server's frame headers byte by byte.
   It asserts: the 101 headers, that the server **never masks** (§5.1), a text round-trip
   including multi-byte UTF-8, a **byte-for-byte binary round-trip** of
   `00 ff fe 01 80 7f c3 28 0d 0a` (not valid UTF-8, not printable), that a ping is answered by a
   pong with the same payload, that two continuation frames are reassembled into one message
   before the handler runs, and that a close frame is echoed with the same big-endian status code.
2. **`websocat` 1.14.1**, a separately built binary, driving the same server end to end — it
   completes the handshake, receives the server's unprompted greeting and gets its own message
   echoed back. Its RFC 6455 implementation is `websocket`/`websocket-base` (rust-websocket),
   which shares no code with the `tokio-tungstenite` this server frames with. **Fails, never
   skips, when websocat is missing.**
3. **RFC 6455 §1.3's published worked example** — key `dGhlIHNhbXBsZSBub25jZQ==` must produce
   `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=`.

## Not implemented

- **TLS (`wss://`)** — put a TLS terminator in front, or use the `tls` protocol.
- **permessage-deflate (RFC 7692)** and every other extension. `Sec-WebSocket-Extensions` is
  ignored rather than negotiated, which is legal (the server simply agrees to none) but means a
  client asking for compression gets none.
- **Autobahn test suite conformance.** Not run. The framing is `tungstenite`'s, which does pass
  it, but that is an inherited claim and not one this protocol has checked.
