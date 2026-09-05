# WebRTC Server Implementation

> **Status: `Experimental` — it works.** A real `RTCPeerConnection` is established with a
> real peer, a data channel opens, and messages cross it in both directions. This is
> asserted end to end in `tests/server/webrtc/e2e_test.rs`, which uses webrtc-rs itself as
> the peer.
>
> It was `Incomplete` until this pass, and the banner that used to be here was accurate:
> `spawn` bound nothing, `accept_offer` had no caller, and no byte could move. What changed
> is that the protocol now owns a signalling transport, so an offer has somewhere to arrive.

## Scope

**Data channels only.** No audio, no video, no media tracks. A media `m=` line in an offer
is reported to the model (`media_kinds`) but never served — the answer carries the data
channel and nothing else. This is a deliberate scope choice: data channels are a complete,
useful protocol on their own, and media would need a whole track/codec pipeline the LLM has
no useful role in.

## Signalling: WebSocket, built in

WebRTC cannot bootstrap itself; SDP has to travel over something else. This server binds a
`TcpListener` on its configured port and speaks a small JSON protocol over WebSocket
(`tokio-tungstenite`, which the `webrtc` feature already declares — no new dependency).

```text
peer   -> {"type":"offer","peer_id":"alice","sdp":{"type":"offer","sdp":"v=0\r\n..."}}
netget -> {"type":"answer","peer_id":"alice","sdp":{"type":"answer","sdp":"v=0\r\n..."}}
netget -> {"type":"rejected","peer_id":"alice","reason":"..."}   # the model declined
netget -> {"type":"error","message":"..."}                       # unusable input
```

`sdp` may also be a bare SDP string.

**Why this and not the sibling `webrtc_signaling` server**: that protocol is a *relay* — it
forwards SDP between two external peers and is itself neither end of the connection. Making
`webrtc` depend on a separately started relay would mean a peer cannot connect unless the
user happened to start two servers, and NetGet has no dependency mechanism that is actually
wired up (`get_dependencies()` is overridden by no protocol). A self-contained endpoint is
startable on its own, testable on its own, and is what a browser peer needs anyway. The two
protocols stay independent; `webrtc_signaling` is not touched or referenced.

**Why not trickle ICE**: the offerer gathers all candidates before sending the offer, and
this server gathers all of its own before answering. That removes the ICE-candidate relay
entirely, which is what lets one WebSocket frame each way be sufficient. It costs some setup
latency (a full gather instead of an incremental one) and it means a peer that trickles will
not interoperate. Browsers can produce a fully-gathered offer by waiting for
`icegatheringstate === "complete"` before posting it.

**One peer per signalling WebSocket.** A second offer on the same socket is refused. The
peer connection's lifetime is tied to the socket: closing the WebSocket closes the
`RTCPeerConnection`. That gives deterministic cleanup for a transport that is otherwise
invisible to the process, and it means an abandoned peer cannot outlive its signalling
channel.

## Division of labour

Rust owns the transport (webrtc-rs: ICE, DTLS, SCTP). The LLM decides content:

| Event | What the model decides | Actions |
|---|---|---|
| `webrtc_offer_received` | whether to admit this peer | `accept_offer`, `reject_offer` |
| `webrtc_peer_connected` | what to say first | `send_message`, `disconnect`, `wait_for_more` |
| `webrtc_message_received` | how to reply | `send_message`, `disconnect`, `wait_for_more` |

No action carries SDP, ICE candidates, base64 or hex. The model sees a *summary* of the
offer — `peer_id`, `remote_addr`, `requests_data_channel`, `media_kinds` — and answers with
a decision. The SDP never leaves Rust.

### The admission path is fail-closed

`decide_offer` (in `mod.rs`) accepts **only** on an explicit `accept_offer`. An LLM error, a
timeout, an empty reply, or a reply containing other actions but no decision all reject.
`reject_offer` carries the model's own reason, so a refusal and a silence are distinguishable
in the frame the peer receives — the structural distinction the OAuth2 defect lacked.
`tests/server/webrtc/e2e_test.rs::test_webrtc_offer_without_decision_is_refused` pins this.

### Failure reaches the peer as a category, never as an error string

The `rejected` reason and the signalling `error` message are peer-visible free text, so
nothing derived from an internal error may be put in them — that is the repo-wide rule in
`src/utils/wire_failure.rs`, and this protocol used to break it in two places (the LLM-error
reason interpolated the `anyhow` chain; the negotiation-failure message interpolated
webrtc-rs internals). Both now send a fixed string and log the error.

The data-channel path used to be the other systemic defect: an LLM error on
`webrtc_peer_connected` / `webrtc_message_received` returned silently, leaving the peer
waiting on a channel that would never answer. It now writes
`WireFailure::prefixed_text()` on the channel — the peer connection stays up, because a
transient overload is not a reason to tear down a negotiated session.

`decision=` tags keep the three outcomes apart in the log, the way `radius` does it:

| tag | meaning |
|---|---|
| `model_reject` | the model explicitly refused |
| `fail_closed_no_decision` | the model answered, with nothing usable in it |
| `fail_closed_backend_overloaded` | the call failed and the backend is saturated (retryable) |
| `fail_closed_backend_error` | the call failed for anything else |

The overloaded/error split is what a protocol with status codes would express as 503 vs 500;
the signalling protocol has no codes, so it survives in the log and in the two distinct
`WireFailure` texts.
`test_webrtc_offer_backend_failure_rejects_without_leaking` pins the no-leak guarantee.

The SDP is also parsed (`RTCSessionDescription::offer`, which unmarshals) *before* the model
is consulted, so an offer webrtc-rs cannot read costs no LLM round-trip and never reaches a
decision.

### `get_async_actions` is empty, on purpose

Server-side async actions are read by nothing in the LLM prompt path — only
`call_llm_for_client` consumes them, and that is the `Client` trait. Declaring
`list_peers`/`send_to_peer` here would advertise a capability the model cannot invoke.
`WebRtcServerData::list_peers` and `send_to_peer` exist as Rust API for a caller that holds
the live server data; if server async actions ever become reachable, wire them through
`execute_action_with_state` + `AppState::register_server_handle`.

## Startup parameters

| Parameter | Default | Effect |
|---|---|---|
| `ice_servers` | `[]` | STUN/TURN URLs. **Empty by default**: host candidates only, which works on localhost and LAN and contacts no third party. The old code hardcoded Google STUN, which meant every start reached out to Google and every test would have too. |
| `max_peers` | 32 | Offers past this limit are refused with a `rejected` frame. |

Both are read. Neither is decorative.

## Per-peer state machine

Idle → Processing → Accumulating → Idle, one machine per peer, in `PeerCtx::handle_peer_event`:

- The first event claims the machine and processes it.
- Events arriving during an LLM call are queued (state `Accumulating`) and drained by the
  task that owns the machine, in order.
- Only one LLM call per peer is ever in flight, so two replies cannot race on one data
  channel. Different peers never block each other.

The peer table lock is never held across an `.await` that does I/O or calls the LLM: every
handler takes the lock, copies what it needs, drops it in an inner scope, then awaits.

## Peers, and their teardown

Each peer holds an `RTCPeerConnection`, its data channel once open, and a `ConnectionId`
registered with the `ServerInstance` (so it appears in the TUI and the access log).
`PeerCtx::teardown` is idempotent and removes the peer, closes the peer connection and drops
the connection record. It is called from three places, whichever gets there first winning:
the peer-connection state callback (`Failed`/`Closed`/`Disconnected`), the signalling
socket's cleanup, and the `disconnect` action.

`spawn` registers the accept loop via `AppState::register_server_task`, so `stop_server`
releases the socket. Per-connection tasks are untracked — the codebase-wide limitation, not
specific to this protocol.

## Peer-controlled input

Everything a peer can send is validated before use, and nothing on this path panics:

- non-JSON, or JSON that is not a known frame → `error` frame, connection continues
- a frame that is not an `offer` → `error` frame
- `peer_id` empty, whitespace-only, or over 128 characters → `error` frame
- `peer_id` already connected → `error` frame
- SDP that does not unmarshal, or whose type is not `offer` → `error` frame
- more peers than `max_peers` → `rejected` frame
- binary or ping/pong frames → refused or ignored, never parsed as signalling

`test_webrtc_malformed_signalling_is_rejected_without_panic` exercises these and asserts the
server did not panic.

## Messages

Inbound: text frames arrive as `message` with `is_binary: false`. Binary frames arrive with
`is_binary: true` and their bytes rendered as **lossy UTF-8** in `message` — the model never
sees raw bytes or hex, and it cannot reconstruct arbitrary binary from what it is shown.
Outbound `send_message` always sends text (`RTCDataChannel::send_text`).

This is honest asymmetry, not a round trip: a binary-heavy peer protocol is out of scope.

## Library

**webrtc-rs 0.11** — pure Rust, full stack, data channels without the media machinery.
`datachannel-rs` (libdatachannel bindings) was rejected for FFI and system-library cost.
`MediaEngine::default()` is used with no codecs registered, which is all a data-channel-only
peer needs. mDNS is left at webrtc-rs's default (`QueryOnly`): remote `.local` candidates are
accepted, local host candidates use real IPs, and a failure to open the mDNS socket is
non-fatal.

Note webrtc-ice excludes loopback addresses from host candidates, so a host with *no*
non-loopback interface cannot complete ICE at all. That is a property of the crate, not of
this code.

## Limitations

1. **No media.** Data channels only.
2. **No trickle ICE.** The peer must gather before offering.
3. **No renegotiation / ICE restart.** Parameters are fixed at offer/answer time.
4. **One peer per signalling socket**, and the peer dies with the socket.
5. **One data channel per peer** is tracked; a peer opening a second channel replaces the
   tracked one for outbound messages.
6. **Binary is lossy** in both directions (see Messages).
7. **No peer authentication beyond the model's decision.** `peer_id` is self-asserted; the
   model is the only gate.
8. **The server's local IPs appear in the SDP answer** — inherent to ICE.

## Example

```text
> open_server webrtc port 9000 "Admit peers whose id starts with guest; echo their messages"

[EVENT] webrtc_offer_received {peer_id: "guest-7", remote_addr: "127.0.0.1:51234",
                               requests_data_channel: true, media_kinds: []}
[ACTION] accept_offer
         -> answer frame sent, ICE + DTLS + SCTP complete

[EVENT] webrtc_peer_connected {peer_id: "guest-7", channel_label: "netget"}
[ACTION] send_message {message: "Welcome to NetGet"}

[EVENT] webrtc_message_received {peer_id: "guest-7", message: "hello", is_binary: false}
[ACTION] send_message {message: "hello back"}
```

## References

- [webrtc-rs](https://docs.rs/webrtc/latest/webrtc/)
- [WebRTC Data Channels (MDN)](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Using_data_channels)
- [Signalling and video calling (MDN)](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Signaling_and_video_calling)
- Tests and their rationale: `tests/server/webrtc/CLAUDE.md`
