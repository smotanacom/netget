# WebRTC Signaling Server Implementation

> **Fixed this session: the relay never relayed anything.**
>
> `handle_connection` moved the WebSocket `SplitSink` into `register_peer`, so after a
> successful `register` it could not keep reading and `break`-ed out of the loop
> immediately (the code even said so: *"ws_tx was consumed, can't continue with this
> connection"*). Breaking ran the disconnect cleanup, which unregistered the peer that had
> just registered and dropped its socket. No peer was ever registered for longer than one
> statement, so `forward_message` could never find a recipient: **not a single offer,
> answer or ICE candidate was ever forwarded.** `SignalingMessage::Registered` and
> `SignalingMessage::Error` were defined but never sent, so a client waiting for
> registration confirmation hung.
>
> The fix is a per-connection writer task. The sink is owned by that task; everything that
> wants to write — this connection's read loop, and any other peer relaying towards it —
> queues on an `mpsc::UnboundedSender<Message>` stored in the peer registry. The read loop
> keeps running for the life of the connection.
>
> Also fixed: the three event types called neither `.with_actions(...)` nor
> `.with_no_actions()`, so `call_llm` offered the model nothing protocol-specific;
> `get_event_types()` returned three *fresh* `EventType`s with `{"type": "placeholder"}`
> response examples unrelated to the statics the server fires; two async actions
> (`list_signaling_peers`, `broadcast_message`) built an `ActionResult::Custom` that
> nothing consumed, so both reported success and did nothing; and the connected/
> disconnected/message events all used `{"type": "no_action"}` as their response example —
> an action that exists nowhere in NetGet, rendered verbatim into the prompt.
>
> Verified with two `websockets` clients: register → `registered`, offer alice→bob,
> answer bob→alice, ICE candidate, undeliverable target → `error`, duplicate peer ID →
> `error` (connection stays open), malformed frame → `error`.

## Overview

WebRTC Signaling Server implementation for NetGet. This server provides **WebSocket-based SDP and ICE candidate relay** to facilitate automatic peer-to-peer WebRTC connections. It eliminates the need for manual SDP exchange, enabling seamless WebRTC communication between peers.

## Purpose

WebRTC requires an out-of-band signaling mechanism to exchange:
1. **SDP offers** - Connection parameters from initiating peer
2. **SDP answers** - Connection response from receiving peer
3. **ICE candidates** - Network path information for NAT traversal

This signaling server acts as a message relay, forwarding these messages between peers so they can establish direct P2P connections.

## Architecture

### Core Components

1. **WebSocket Server**: Accepts WebSocket connections from peers
2. **Peer Registry**: Tracks connected peers by unique peer IDs
3. **Message Relay**: Forwards signaling messages between registered peers
4. **LLM Monitoring**: Optionally monitors and logs signaling flow

### Protocol Stack

```
Application (SDP/ICE exchange)
    ↓
WebSocket (bidirectional messaging)
    ↓
TCP
    ↓
IP
```

## Library Choices

### tokio-tungstenite (v0.21)

**Rationale**: Pure Rust async WebSocket implementation

**Pros**:
- Integrates seamlessly with tokio async runtime
- Lightweight and efficient
- No C dependencies
- Well-maintained and widely used

**Cons**:
- Requires async/await understanding
- WebSocket protocol adds overhead vs raw TCP

**Alternative Considered**: Manual TCP with custom framing - rejected for complexity

## Signaling Protocol

### Message Types

All messages are JSON objects with a `type` field:

#### 1. Register

**From**: Client → Server
**Purpose**: Register with a unique peer ID

```json
{
  "type": "register",
  "peer_id": "alice"
}
```

**Response** — actually sent, immediately, before the `webrtc_signaling_peer_connected`
event fires:
```json
{
  "type": "registered",
  "peer_id": "alice"
}
```

Registering twice on one connection, or claiming a peer ID that is already taken, replies
with an `error` and leaves the connection open.

#### 2. Offer

**From**: Peer A → Server → Peer B
**Purpose**: Initiate WebRTC connection

```json
{
  "type": "offer",
  "from": "alice",
  "to": "bob",
  "sdp": {
    "type": "offer",
    "sdp": "v=0\r\no=- ... [full SDP]"
  }
}
```

#### 3. Answer

**From**: Peer B → Server → Peer A
**Purpose**: Accept WebRTC connection

```json
{
  "type": "answer",
  "from": "bob",
  "to": "alice",
  "sdp": {
    "type": "answer",
    "sdp": "v=0\r\no=- ... [full SDP]"
  }
}
```

#### 4. ICE Candidate

**From**: Peer A ↔ Server ↔ Peer B
**Purpose**: Exchange network paths

```json
{
  "type": "ice_candidate",
  "from": "alice",
  "to": "bob",
  "candidate": {
    "candidate": "candidate:... [ICE candidate string]",
    "sdpMLineIndex": 0,
    "sdpMid": "0"
  }
}
```

#### 5. Error

**From**: Server → Client
**Purpose**: Report errors

```json
{
  "type": "error",
  "message": "Peer ID already registered"
}
```

## Connection Flow

### Basic Signaling Flow

```
1. Alice connects to signaling server
   → WebSocket: ws://localhost:8080
   → Sends: {"type": "register", "peer_id": "alice"}

2. Bob connects to signaling server
   → WebSocket: ws://localhost:8080
   → Sends: {"type": "register", "peer_id": "bob"}

3. Alice initiates WebRTC connection
   → Generates SDP offer locally
   → Sends: {"type": "offer", "from": "alice", "to": "bob", "sdp": {...}}
   → Server forwards to Bob

4. Bob receives offer and responds
   → Generates SDP answer locally
   → Sends: {"type": "answer", "from": "bob", "to": "alice", "sdp": {...}}
   → Server forwards to Alice

5. ICE candidates exchanged (both directions)
   → Alice/Bob send candidates as they're discovered
   → Server forwards each to the other peer

6. WebRTC connection established
   → Peers connect directly (P2P)
   → Signaling connection can be closed (but often kept open)
```

## LLM Integration

### Events

1. **`webrtc_signaling_peer_connected`** - Peer registered with server
    - Provides peer_id and remote_addr
    - LLM can monitor which peers are online

2. **`webrtc_signaling_peer_disconnected`** - Peer disconnected
    - Provides peer_id
    - LLM can track peer lifecycle

3. **`webrtc_signaling_message_received`** - Signaling message received
    - Provides peer_id, message_type, target_peer
    - LLM can monitor signaling flow

### Actions

#### User-Triggered (Async)

None. `list_signaling_peers` and `broadcast_message` were removed: both returned an
`ActionResult::Custom` that nothing in this protocol consumed.

#### Event-Triggered (Sync)

Available on `webrtc_signaling_peer_connected` only, where a socket still exists to act on:

- **`send_signaling_message`** — send one JSON signaling message to this peer. Must be a
  valid message (`register`, `registered`, `offer`, `answer`, `ice_candidate`, `relay`,
  `error`). Written to the peer's WebSocket as a text frame.
- **`disconnect_peer`** — close this peer's signaling connection.

The other two events are declared `.with_no_actions()`, deliberately:

- `webrtc_signaling_peer_disconnected` — the socket is already gone.
- `webrtc_signaling_message_received` — the message has *already been relayed* by the time
  this fires, and the LLM call is spawned off the connection task so observation never
  delays relay. Putting a model round-trip in front of every ICE candidate would break any
  real browser peer.

Both can still use the common actions (`set_memory`, `append_memory`, `show_message`,
`append_to_log`), which is how a handler records signaling flow.

### When the LLM call fails

All three call sites used to swallow the error (`if let Ok(..)` / `let _ =`). Only one of the
three can honestly answer the peer, and the asymmetry follows directly from the action lists
above:

- `webrtc_signaling_peer_connected` — the peer may be waiting on what the model decides, so it
  is sent `{"type": "error", "message": "Server-side handler for peer '<id>' failed; …"}`.
  Registration itself is not undone: it succeeded, and `registered` was already sent. An
  `error` frame is the only thing this server may put on the wire on the model's behalf; it
  must never synthesise an `offer`, `answer` or `registered` the model did not produce.
- `webrtc_signaling_message_received` — **deliberately silent.** It is `.with_no_actions()` and
  fires after the relay has already happened and already been reported, so the model cannot
  speak to the peer even on the success path. An error frame here would announce a failure the
  peer's signaling did not suffer, and a real browser peer could abort a negotiation that in
  fact succeeded.
- `webrtc_signaling_peer_disconnected` — silent because the socket is already gone. Cleanup
  still runs.

All three log at ERROR on both channels (`error!` plus a `[ERROR]` status message).
`tests/server/webrtc_signaling/llm_failure_test.rs` asserts the error frame, the silence, and
that the relay itself is unaffected.

## State Management

### Peer Tracking

Each connected peer is tracked with:
- **peer_id**: Unique identifier (chosen by peer, unauthenticated)
- **out_tx**: `mpsc::UnboundedSender<Message>` into that connection's writer task — *not*
  the `SplitSink` itself. Storing the sink is what broke the relay; see the banner.
- **remote_addr**: TCP address of peer
- **connection_id**: NetGet connection tracking ID

Peer state lives only in this in-memory map for the lifetime of the process. Protocols must
not implement storage, and this one does not: there is no peer database, no queue and no
persistence.

### Message Forwarding

Messages are forwarded based on the `to` field:
1. Parse incoming message
2. Look up recipient by peer_id
3. Queue the message on the recipient's `out_tx`; its writer task sends it
4. If the recipient is not registered, the message is dropped and the *sender* gets
   `{"type": "error", "message": "Cannot deliver <kind> to <peer>: ..."}`. Nothing is
   queued for later delivery.
5. Fire `webrtc_signaling_message_received` with `delivered` true/false — after the fact

## Usage Patterns

### Pattern 1: Browser ↔ NetGet WebRTC Server

```
Browser (JavaScript)           Signaling Server           NetGet WebRTC Server
    |                                 |                            |
    |--- register (peer: "browser") >|                            |
    |                                 |< register (peer: "netget")-|
    |                                 |                            |
    |--- offer (to: "netget") ------->|                            |
    |                                 |--- offer (from: "browser")>|
    |                                 |                            |
    |                                 |<--- answer (to: "browser")-|
    |<--- answer (from: "netget") ----|                            |
    |                                 |                            |
    |<=========== P2P Data Channel ===========>|
```

### Pattern 2: Two NetGet Instances

```
NetGet Client A      Signaling Server      NetGet Client B
    |                       |                      |
    |--- register("A") ---->|                      |
    |                       |<--- register("B") ---|
    |                       |                      |
    |--- offer(to:"B") ---->|                      |
    |                       |--- offer(from:"A") ->|
    |                       |                      |
    |                       |<-- answer(to:"A") ---|
    |<-- answer(from:"B") --|                      |
    |                       |                      |
    |<======== P2P Data Channel ========>|
```

## Implementation Details

### WebSocket Upgrade

```rust
// Accept TCP connection
let stream = listener.accept().await?;

// Upgrade to WebSocket
let ws_stream = accept_async(stream).await?;

// Split into sender/receiver
let (ws_tx, ws_rx) = ws_stream.split();
```

### Peer Registration

```rust
// One writer task owns the sink; the registry only holds a channel into it.
let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
tokio::spawn(async move {
    while let Some(msg) = out_rx.recv().await {
        if ws_tx.send(msg).await.is_err() { break; }
    }
    let _ = ws_tx.close().await;
});

peers.insert(peer_id.clone(), PeerConnection { peer_id, out_tx, remote_addr, connection_id });
```

### Message Forwarding

```rust
// Find recipient and queue on its writer task
let peer_conn = peers.get(to).context("Peer not found")?;
peer_conn.out_tx.send(Message::Text(msg_json))?;
```

## Limitations

1. **No Authentication**: Any peer can register any peer_id (first-come-first-served)
2. **No Encryption**: WebSocket is not encrypted (use wss:// in production)
3. **No Persistence**: peer registry is in-memory only, and deliberately so — protocols
   must not implement storage
4. **No Reconnection**: Peers must re-register if disconnected
5. **No Message Queue**: if the recipient is not registered the message is dropped and
   the sender is told so; nothing is stored for later delivery
6. **Single Server**: No clustering or load balancing

## Security Considerations

This is an **unauthenticated relay by design** — a honeypot-shaped SDP exchange with no
identity model. What that does and does not mean is worth being exact about, because two
things that read as consequences of "no authentication" were separate defects, fixed
September 2026 and pinned by `tests/server/webrtc_signaling/relay_abuse_test.rs`:

**Still true, and deliberate**

- **Peer ID squatting**: any client may claim any *unused* peer_id, first come first served,
  and no peer can verify who it is talking to. A duplicate is refused, and that refusal is
  the only thing standing between two peers and each other's traffic.
- **No authorization**: no control over who may signal whom. Relay is never gated on the
  model — it happens in Rust before any handler runs, because a model round-trip in front of
  every ICE candidate would break any real browser peer. The LLM's only lever is
  `disconnect_peer` at registration time.
- **Eavesdropping**: SDP carries IP addresses, and this is plain `ws://`. There is no `wss://`
  listener.
- **No rate limiting**: a registered peer may relay as fast as it likes, and the
  per-connection writer queue is an unbounded channel, so a peer that stops reading
  accumulates whatever others send it. Put this behind something that limits connections on
  an untrusted network.

**Fixed, because neither followed from the above**

- **`from` was forgeable.** The field was relayed verbatim, so any registered peer could send
  an offer that the recipient *and* the `webrtc_signaling_message_received` event attributed
  to a third party — and the recipient answers whoever `from` names. It is now overwritten
  with the sender's registered id (a mismatch is logged at WARN, not refused: the field is
  redundant, since the connection already identifies the sender).
- **An unregistered socket could relay.** The offer/answer/ice_candidate/relay arm had no
  registration guard, so a connection that never sent `register` could inject frames —
  including a `relay` carrying arbitrary JSON — at any registered peer. It now gets an error
  frame saying to register first.

**Limits that now exist** (there were none):

| Limit | Value | Why |
|---|---|---|
| Registered peers | 1024 | the registry is keyed on a string the peer chooses |
| Peer id length | 128 bytes | echoed into logs, events and every relayed frame |
| WebSocket message | 256 KiB | tungstenite's default is **64 MiB**, and the largest real SDP offer is a few kilobytes |
| Handshake | 10s | a TCP connection that never upgraded parked a task forever, invisible to the dashboard since a connection is only registered in `AppState` once it registers |

**Still recommended before any untrusted exposure**: peer authentication, per-peer rate
limiting, and WSS.

## Testing Strategy

See `tests/server/webrtc_signaling/CLAUDE.md` for testing details.

Key test scenarios:
- Two peers register and exchange offers/answers
- ICE candidate forwarding
- Peer disconnection handling
- Duplicate peer ID handling
- Message forwarding errors

## Performance Considerations

- **Memory**: a peer's floor is small, but its ceiling is `SIGNALING_MAX_MESSAGE_BYTES`
  (256 KiB) per in-flight frame plus an **unbounded** writer queue, so a peer that stops
  reading is bounded only by what others send it. This section used to say "~1 KB per peer"
  while the frame limit was tungstenite's 64 MiB default.
- **CPU**: Minimal (message forwarding is fast)
- **Connections**: hard limit **1024** (`SIGNALING_MAX_PEERS`). This used to read
  "Recommended limit: 1,000-10,000 concurrent peers" while nothing enforced anything.
- **Bandwidth**: Depends on signaling traffic (typically < 10 KB/peer)

## Future Enhancements

1. **Authentication**: Token-based peer verification
2. **Persistence**: Redis/database backend for peer registry
3. **Clustering**: Multi-server signaling with shared state
4. **Message Queue**: Offline message delivery
5. **Presence**: Track online/offline status
6. **Rooms**: Group peers into rooms/channels
7. **Broadcast**: Send to all peers in a room
8. **Metrics**: Track signaling statistics
9. **Rate Limiting**: Per-peer message throttling
10. **TLS/WSS**: Encrypted WebSocket connections

## Example Usage

```bash
# Start signaling server
> open_server webrtc-signaling 0.0.0.0:8080 "WebRTC signaling relay"

# Server displays: "WebRTC Signaling server listening on 0.0.0.0:8080"

# Peer A connects (WebSocket client or NetGet)
> [EVENT] webrtc_signaling_peer_connected (peer_id: "alice", remote_addr: "127.0.0.1:54321")

# Peer B connects
> [EVENT] webrtc_signaling_peer_connected (peer_id: "bob", remote_addr: "127.0.0.1:54322")

# Alice sends offer to Bob
> [Forwarded] offer from alice to bob

# Bob sends answer to Alice
> [Forwarded] answer from bob to alice

# Connection established
> # Peers now have direct P2P connection, signaling complete

# Peer disconnects
> [EVENT] webrtc_signaling_peer_disconnected (peer_id: "bob")
```

## Integration with WebRTC Server/Client

The signaling server works with:
- **WebRTC Server** (`src/server/webrtc/`): Peers can signal to NetGet server instances
- **WebRTC Client** (`src/client/webrtc/`): NetGet clients can use signaling for automatic SDP exchange
- **External Peers**: Any WebSocket client (browsers, other apps)

## References

- [WebSocket Protocol (RFC 6455)](https://tools.ietf.org/html/rfc6455)
- [WebRTC Signaling](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Signaling_and_video_calling)
- [Perfect Negotiation Pattern](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Perfect_negotiation)
- [tokio-tungstenite Documentation](https://docs.rs/tokio-tungstenite/)
