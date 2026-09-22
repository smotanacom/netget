# BitTorrent Peer Wire Protocol - Implementation

## Overview

The BitTorrent Peer Wire Protocol is a TCP-based protocol for peer-to-peer data transfer between BitTorrent clients.
This implementation provides a fully LLM-controlled peer/seeder that can handle handshakes, choke/unchoke, bitfield
exchange, and piece requests.

## Protocol Specification

- **Base Protocol**: TCP (connection-oriented)
- **Encoding**: Binary messages with length prefixes
- **Handshake**: Fixed 68-byte format with protocol string and IDs
- **Port**: Typically 51413 or 6881-6889 (user-configurable)
- **RFC/BEP**: BEP 3 (The BitTorrent Protocol Specification)

## Architecture

### Server Implementation (`mod.rs`)

**Library Choice**: Pure Tokio implementation

- No external peer library (binary protocol is straightforward)
- `tokio::net::TcpListener` for accepting connections
- `tokio::io::split()` for read/write halves
- Manual binary message parsing/encoding

**Key Components**:

```rust
pub struct TorrentPeerServer;

impl TorrentPeerServer {
    pub async fn spawn_with_llm_actions(...) -> Result<SocketAddr>
    async fn handle_connection(...) -> Result<()>
    async fn process_llm_response(...) -> Result<()>
    fn parse_handshake(data: &[u8]) -> Result<(String, String, String)>
    fn parse_message(data: &[u8]) -> Result<(String, serde_json::Value)>
}
```

**Connection Flow**:

1. Accept TCP connection
2. Append every read into a `pending` buffer and drain complete frames from it
3. First 68 bytes are the handshake (protocol string, info_hash, peer_id)
4. Fire `peer_handshake`; write whatever the handler returned
5. Then, repeatedly, while `pending` holds a whole `<4-byte length><body>` frame:
    - Parse message type and payload
    - Fire the matching event
    - Write every output the handler returned, in order
6. Connection persists until closed by peer or error

**Framing (this is the part that used to be wrong).** The peer wire protocol is
length-prefixed over a byte stream, so a `read()` is not a message. The previous
implementation parsed each read as exactly one frame, which meant:

- the bitfield a client sends in the *same segment* as its handshake was discarded (the
  handshake parser looked at `data[..68]` and threw the rest away) — and sending
  handshake + bitfield + interested in one write is what every real client does;
- two coalesced messages became one, the second silently lost;
- a message split across two reads produced "Incomplete message", was dropped, and left
  the stream misaligned so every byte after it was garbage.

`pending` is capped at 2 MiB; a peer that never completes a frame is disconnected rather
than allowed to grow the buffer without bound.

### Binary Message Format

**Handshake** (68 bytes, sent once at connection start):

```
<pstrlen><pstr><reserved><info_hash><peer_id>

pstrlen: 1 byte = 19
pstr: 19 bytes = "BitTorrent protocol"
reserved: 8 bytes (extension bits, usually zeros)
info_hash: 20 bytes (SHA-1 hash of torrent info dictionary)
peer_id: 20 bytes (client identifier, e.g., "-NT0001-xxxxxxxxxxxx")
```

**Messages** (after handshake):

```
<length><message_id><payload>

length: 4 bytes (big-endian) = 1 + payload length
message_id: 1 byte (0-9 = standard messages)
payload: Variable length (depends on message type)
```

**Keep-alive**: `<length=0>` (4 zero bytes, no message_id or payload)

### Message Types

| ID | Name           | Payload                             | Description                     |
|----|----------------|-------------------------------------|---------------------------------|
| 0  | choke          | None                                | Choking peer (stop requests)    |
| 1  | unchoke        | None                                | Unchoking peer (allow requests) |
| 2  | interested     | None                                | Interested in peer's pieces     |
| 3  | not_interested | None                                | Not interested                  |
| 4  | have           | 4 bytes (piece index)               | Announce piece availability     |
| 5  | bitfield       | Variable (bitfield bytes)           | Announce all piece availability |
| 6  | request        | 12 bytes (index, begin, length)     | Request piece block             |
| 7  | piece          | 8+ bytes (index, begin, block data) | Send piece block                |
| 8  | cancel         | 12 bytes (index, begin, length)     | Cancel request                  |

### LLM Actions (`actions.rs`)

**Protocol Trait Implementation**: `Server` trait from `crate::llm::actions::protocol_trait`

Every event type calls `.with_actions(...)` and `.with_parameters(...)`. Until they did,
`call_llm` advertised none of these actions — it builds the model's tool list from
`event.event_type.actions`, not from `get_sync_actions()` — so every peer-wire action the
model produced was rejected as unknown.

The peer wire protocol has no correlation id and no request/response pairing beyond the
handshake: any message is legal at any point after it. So every event advertises the full
action set rather than a narrowed one. What *is* correlated is the handshake's `info_hash`
(the reply must echo it or the peer disconnects) and a request's `index`/`begin` (a piece
carrying different values is discarded). Static handlers echo them with
`{{event.info_hash}}`, `{{event.index}}`, `{{event.begin}}`.

`send_interested`, `send_not_interested` and `send_keepalive` were implemented in
`execute_action` but missing from `get_sync_actions()`, so they were undiscoverable; they
are now declared.

**Sync Actions** (network-triggered):

1. **send_handshake** - Respond to peer handshake
    - Parameters: `info_hash` (hex, 40 chars), `peer_id` (20 ASCII bytes, optional),
      `peer_id_hex` (40 hex chars, optional, takes precedence)
    - Output: 68-byte binary handshake
    - Example:
   ```json
   {
     "type": "send_handshake",
     "info_hash": "0123456789abcdef0123456789abcdef01234567",
     "peer_id": "-NT0001-xxxxxxxxxxxx"
   }
   ```

2. **send_choke** - Choke peer (stop accepting requests)
    - Parameters: None
    - Output: `00 00 00 01 00`
    - Example: `{"type": "send_choke"}`

3. **send_unchoke** - Unchoke peer (allow requests)
    - Parameters: None
    - Output: `00 00 00 01 01`
    - Example: `{"type": "send_unchoke"}`

4. **send_interested** - Express interest in peer's pieces
    - Parameters: None
    - Output: `00 00 00 01 02`

5. **send_not_interested** - No interest in peer's pieces
    - Parameters: None
    - Output: `00 00 00 01 03`

6. **send_have** - Announce piece availability
    - Parameters: `piece_index` (number)
    - Output: `00 00 00 05 04 <index>`
    - Example:
   ```json
   {
     "type": "send_have",
     "piece_index": 0
   }
   ```

7. **send_bitfield** - Announce all pieces
    - Parameters: `bitfield` (hex string, e.g., "ff" = all pieces)
    - Output: `<length> 05 <bitfield bytes>`
    - Example:
   ```json
   {
     "type": "send_bitfield",
     "bitfield": "ff"
   }
   ```
    - **Bitfield Encoding**: Each bit represents one piece (1 = have, 0 = don't have). Bits are big-endian within bytes.

8. **send_piece** - Send piece data
    - Parameters: `index` (piece index), `begin` (byte offset), `block_hex` (data in hex)
    - Output: `<length> 07 <index> <begin> <block data>`
    - Example:
   ```json
   {
     "type": "send_piece",
     "index": 0,
     "begin": 0,
     "block_hex": "48656c6c6f20576f726c64"
   }
   ```

9. **send_keepalive** - Keep connection alive
    - Parameters: None
    - Output: `00 00 00 00`

**Event Types** (incoming messages):

Every message event carries `message_type`, naming the message that actually arrived.

1. **peer_handshake** - Peer initiates connection
    - Payload: `{info_hash, peer_id, peer_id_hex}`
    - `peer_id` is lossy UTF-8 and may contain replacement characters: a peer ID is 20
      arbitrary bytes and the trailing random section usually is not text. `peer_id_hex`
      is the faithful form; never echo `peer_id` back for a non-ASCII peer.
    - Must respond with send_handshake echoing `{{event.info_hash}}`

2. **peer_choke_message** - Peer state change
    - Fires for choke, unchoke, interested *and* not_interested
    - Payload: `{message_type}`

3. **peer_request_message** - Peer requests piece block
    - Payload: `{message_type, index, begin, length}`
    - Should respond with send_piece echoing `index` and `begin`

4. **peer_bitfield_message** - Peer announces pieces
    - Payload: `{message_type, bitfield}`

5. **peer_message** - have, piece, cancel, keep-alive, or an undecoded message id
    - Payload: `{message_type}` plus whichever of `piece_index`, `index`, `begin`,
      `block_hex`, `id`, `payload_hex` apply
    - This event is new. `have`, `piece`, `cancel`, keep-alives and unknown ids used to be
      announced to the model as `peer_choke_message`, which is simply false — as was
      routing `interested` there while calling it a "choke message".

### Message Parsing

**parse_handshake()**:

```rust
// Validate length (min 68 bytes)
// Check pstrlen == 19
// Check pstr == "BitTorrent protocol"
// Extract info_hash: bytes[28..48] → hex string
// Extract peer_id: bytes[48..68] → UTF-8 string
```

**parse_message()**:

```rust
// Read length (4 bytes, big-endian)
// If length == 0: keepalive
// Read message_id (1 byte)
// Parse payload based on message_id:
//   - 0-3: No payload
//   - 4: Have (4 bytes piece index)
//   - 5: Bitfield (variable bytes)
//   - 6: Request (12 bytes: index, begin, length)
//   - 7: Piece (8+ bytes: index, begin, block data)
//   - 8: Cancel (12 bytes: index, begin, length)
```

### Message Encoding

**Handshake**:

```rust
let mut handshake = Vec::new();
handshake.push(19u8);                              // pstrlen
handshake.extend_from_slice(b"BitTorrent protocol");
handshake.extend_from_slice(&[0u8; 8]);           // reserved
handshake.extend_from_slice(&info_hash);           // 20 bytes
handshake.extend_from_slice(peer_id.as_bytes());   // 20 bytes
```

**Messages**:

```rust
let mut message = Vec::new();
message.extend_from_slice(&length.to_be_bytes());  // 4 bytes
message.push(message_id);                          // 1 byte
message.extend_from_slice(&payload);               // Variable

// Example: Piece message
// length = 9 + block.len()
// message_id = 7
// payload = index (4) + begin (4) + block (variable)
```

## LLM Integration

### Instruction Guidelines

**Example Instruction (Seeder)**:

```
You are a BitTorrent seeder. You have all pieces for any torrent. Respond to handshakes with your peer ID "-NT0001-xxxxxxxxxxxx". Send bitfield "ff" (all pieces). When peers request pieces, send the requested data. Keep all peers unchoked.
```

**Example Instruction (Leecher)**:

```
You are a BitTorrent leecher. You have no pieces initially. Respond to handshakes. Send interested message. When unchoked, request pieces sequentially starting from piece 0. Track downloaded pieces.
```

**Behavior Control**:

- **Choking Strategy**: Control when to choke/unchoke peers (e.g., tit-for-tat, optimistic unchoking)
- **Piece Selection**: Rarest first, sequential, random
- **Request Queue**: Pipeline multiple requests (typical: 5-10 outstanding)
- **Upload Rate**: Control how fast to serve pieces (delay between send_piece calls)

### Typical LLM Response Flow

**Connection Start**:

1. Peer sends handshake
2. LLM receives: `{info_hash: "abc...", peer_id: "xyz..."}`
3. LLM returns: `{type: "send_handshake", info_hash: "abc...", peer_id: "-NT0001-..."}`
4. LLM may also send: `{type: "send_bitfield", bitfield: "ff"}` or `{type: "send_unchoke"}`

**Piece Request**:

1. Peer sends: Request message
2. LLM receives: `{index: 0, begin: 0, length: 16384}`
3. LLM checks: Do I have piece 0? Is peer unchoked?
4. LLM returns: `{type: "send_piece", index: 0, begin: 0, block_hex: "..."}`

**Bitfield Exchange**:

1. Peer sends: Bitfield message
2. LLM receives: `{bitfield: "ff00..."}`
3. LLM analyzes: Peer has pieces [0-7], missing [8+]
4. LLM returns: `{type: "send_interested"}` (if peer has pieces we need)

## Dashboard injection (peer handle + counters)

Every connection registers a peer handle (`peer_support::register_peer_channel` +
`spawn_peer_command_task`) right after `handle_connection` starts, so the dashboard's
`[ message this peer ]` / `[ disconnect this peer ]` rows work; the handle is removed on
every exit path through the single cleanup at the end of `handle_connection`. Every wire
verb returns `ActionResult::Output`, so the generic peer command task encodes an injected
`send_unchoke` / `send_piece` with exactly the code the LLM path uses — no Custom-result
gap. `execute_action` gains a `close_connection => ActionResult::CloseConnection` arm (not
offered to the model) so `[ disconnect this peer ]` half-closes the write side; the reader's
`read() == 0` path then runs teardown.

Read and write both call `update_connection_stats`, so the rail's `↓ ↑` counters move.
Note that injected sends go through the generic peer task, which does not touch the counters.

## Connection State Tracking

`protocol_info` is `ProtocolConnectionInfo::empty()`. `ProtocolConnectionInfo` is a generic
`serde_json::Value` wrapper (`state/server.rs`), not the per-protocol enum earlier versions
of this file described: there is no `TorrentPeer` variant carrying a write half, a state
machine or the negotiated info_hash.

**There is no per-connection Idle/Processing/Accumulating state machine here.** The
connection task reads, calls the LLM, writes, and reads again — strictly serially — so
concurrent LLM calls on one connection are impossible by construction, but data arriving
during a call is not queued and dispatched separately; it simply waits in the socket
buffer and is drained on the next read. Earlier versions of this file described a state
machine that was never implemented.

## Logging Strategy

**DEBUG Level**:

- Connection accepted/closed
- Handshake parsed (info_hash, peer_id)
- Message type identified
- LLM call initiated
- Bytes sent/received

**TRACE Level**:

- Full binary data (hex)
- Parsed message details

**INFO Level**:

- LLM-generated messages

**ERROR Level**:

- Accept errors
- Parse errors (invalid handshake, malformed messages)
- LLM call failures

## Limitations

1. **No Piece Storage**: LLM doesn't have actual torrent files. Piece data is fake/random unless LLM explicitly tracks
   it.

2. **No SHA-1 Verification**: Piece hashes are not validated. LLM can send any data for pieces.

3. **No Fast Extension (BEP 6)**: Fast peer messages not supported (have_all, have_none, suggest_piece, reject_request,
   allowed_fast).

4. **No Extension Protocol (BEP 10)**: No extension handshake, no DHT/PEX/metadata exchange.

5. **No Encryption (BEP 29/47)**: No message stream encryption.

6. **Stateless Pieces**: Server restart = lost piece data (unless LLM uses filesystem).

7. **Single-threaded LLM**: the connection task blocks on each LLM call, so a high piece
   request rate serialises behind it. Use a script or static handler for anything
   throughput-sensitive.

8. **No SHA-1 anything**: `info_hash` is never verified against piece data, and piece
   blocks are whatever the handler supplies.

## Security Considerations

- **Info Hash Validation**: LLM should track which torrents it's serving. Reject unknown info_hashes.
- **Piece Index Bounds**: Validate piece indices against torrent metadata (num_pieces).
- **Request Limits**: Limit outstanding requests per peer (typical: 5-10).
- **Upload Rate Limiting**: Prevent bandwidth exhaustion (LLM could implement via delays).
- **Connection Limits**: Limit total peer connections (configurable in instruction).

## Protocol Extensions

**Future Enhancements**:

1. **BEP 6 (Fast Extension)**: Reduce latency with have_all, have_none, suggest_piece
2. **BEP 10 (Extension Protocol)**: Support DHT, PEX, ut_metadata
3. **BEP 29 (uTP)**: UDP-based transport
4. **BEP 47 (Padding)**: Traffic obfuscation

## Testing

See `tests/server/torrent_peer/CLAUDE.md` for comprehensive testing documentation.

## Piece Transfer Example

**Typical Flow**:

1. Peer A (seeder) connects to Peer B (leecher)
2. A sends: Handshake + Bitfield (all pieces) + Unchoke
3. B sends: Handshake + Interested
4. B sends: Request (piece 0, begin 0, length 16384)
5. A sends: Piece (piece 0, begin 0, block data)
6. B sends: Request (piece 0, begin 16384, length 16384)
7. A sends: Piece (piece 0, begin 16384, block data)
8. ... (repeat until piece complete)
9. B sends: Have (piece 0)

**Block Size**: Typically 16 KiB (16384 bytes). Last block may be smaller.

**Piece Size**: Typically 256 KiB - 1 MiB. Configurable in .torrent file.

## References

- [BEP 3: The BitTorrent Protocol Specification](http://www.bittorrent.org/beps/bep_0003.html)
- [BEP 6: Fast Extension](http://www.bittorrent.org/beps/bep_0006.html)
- [BEP 10: Extension Protocol](http://www.bittorrent.org/beps/bep_0010.html)
- [BitTorrent Peer Wire Protocol](https://wiki.theory.org/BitTorrentSpecification#Peer_wire_protocol_.28TCP.29)
- [Piece Picking Algorithms](https://www.bittorrent.org/bittorrentecon.pdf)

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever, and a
hundred of them was a free denial of service on a server that would happily accept a hundred
more. It now declares both halves; the constants and the reasoning live beside them in
`src/server/torrent_peer/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `HANDSHAKE_READ_TIMEOUT` | 30s | BEP 3 has the initiating peer send its 68-byte handshake immediately — it is the first thing on the wire and nothing precedes it — so a peer that has connected and sent nothing is not mid-handshake, it is holding a socket. |
| `IDLE_AFTER_HANDSHAKE_TIMEOUT` | 180s | The peer wire protocol's own answer: the keep-alive is a zero-length message sent roughly every two minutes precisely so an idle-but-live peer can be told from a dead one, and mainline clients drop a connection quiet for about that long. Three minutes gives a conforming peer a full missed keep-alive of slack. |
| `MAX_CONNECTIONS` | 256 | Real swarms are far smaller — mainline clients cap global peers in the low hundreds. Refusal: **a plain close.** BEP 3 has no busy, error or free-text message of any kind, and the one refusal it does define (`CHOKE_FRAME`) is legal only *after* a handshake this peer has not sent. Any bytes here would be read as the first 5 of the 68 handshake bytes, so a client would report a malformed handshake rather than a full server — strictly worse than silence. |

**NetGet's own peer-wire client is the *connected-and-silent* case, and this bound is therefore
wrong as it stands:** `src/client/torrent_peer/mod.rs` connects and its first act is
`read_exact` on the *peer's* 68 bytes; its own handshake is written only by the
`send_handshake` action, so both ends wait and at 30s the server drops a peer the operator is
still looking at. It wants 300s with declared `first_byte_timeout_secs`/`idle_timeout_secs`, as
`src/server/redis/` has — `PROTOCOL_QUALITY.md`'s three-state test.

**The deadline covers the read and nothing else.** The deadline wraps the `read()` call in this protocol's own loop, and everything that can legitimately take minutes happens after it returns. The LLM round-trip, and a `manual`
rule parking an event for a human (`src/state/intercepts.rs`, 300s by default), are outside
every deadline here, so an answer that takes minutes can never close the connection it is an
answer for. That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse:
TFTP evicted live transfers because "idle" was measured wrongly.

`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is removed;
`tests/accept_bounded_test.rs` drives the shared helper, including the guarantee that a busy
connection is never reported as idle.
