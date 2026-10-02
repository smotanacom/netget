# BitTorrent Peer Wire Protocol Client Implementation

## Overview

The BitTorrent Peer Wire Protocol client provides LLM-controlled peer-to-peer data exchange for BitTorrent file
transfers. This is the core protocol used between BitTorrent clients to exchange torrent pieces.

## Protocol Details

**Protocol:** BitTorrent Peer Wire Protocol (BEP 3, BEP 10)
**Transport:** TCP with binary message framing
**Port:** Typically 6881-6889 (configurable)
**Stack:** ETH > IP > TCP > BitTorrent-PeerWire

## Implementation

### Library Choices

- **TCP Socket:** `tokio::net::TcpStream` - Async TCP connections
- **Binary Protocol:** Manual parsing of message frames

### Architecture

1. **Persistent Connection:** Long-lived TCP connection with peer
2. **Handshake:** Initial handshake exchange (68 bytes)
3. **Message Loop:** Continuous exchange of peer wire messages
4. **LLM Integration:** LLM controls piece requests, choking, and data exchange

### Connection Flow

```
1. TCP connection established
2. Handshake exchange (info_hash, peer_id)
3. Optional extension handshake (BEP 10)
4. Peer wire message loop:
   - bitfield exchange
   - interested/not interested
   - choke/unchoke
   - piece requests
   - piece data transfers
5. Connection maintained until disconnect
```

### Message Format

**Handshake (68 bytes):**

```
<pstrlen=19><pstr="BitTorrent protocol"><reserved=8 bytes><info_hash=20 bytes><peer_id=20 bytes>
```

**Peer Wire Messages:**

```
<length prefix=4 bytes><message ID=1 byte><payload>
```

**Message Types:**

- 0: choke
- 1: unchoke
- 2: interested
- 3: not interested
- 4: have (piece index)
- 5: bitfield (piece availability bitmap)
- 6: request (piece index, offset, length)
- 7: piece (piece index, offset, block data)
- 8: cancel
- 9: port (DHT port)

## LLM Control Points

### Actions

1. **peer_handshake** - Send handshake to peer
    - Parameters: info_hash, peer_id
    - LLM decides: identity and torrent to exchange

2. **peer_interested** - Express interest in peer's pieces
    - LLM decides: when to show interest

3. **peer_not_interested** - Express lack of interest
    - LLM decides: when interest is lost

4. **peer_request_piece** - Request a piece from peer
    - Parameters: index, begin, length
    - LLM decides: piece selection strategy (rarest-first, sequential, etc.)

5. **peer_send_piece** - Send piece data to peer
    - Parameters: index, begin, block
    - LLM decides: upload strategy, rate limiting

6. **disconnect** - Close peer connection

### Events

1. **peer_handshake** - Received handshake from peer
    - Data: info_hash, peer_id, reserved bytes
    - LLM analyzes: peer identity, torrent match

2. **peer_message** - Received peer wire message
    - Data: message_type, payload_len, payload_hex
    - LLM analyzes: piece availability, choke state, received pieces

## Dashboard injection (command channel)

The client registers a command channel (`command_support::register_command_channel`) right
after the stream split — before the connected-event LLM call, which a manual rule can park.
The read loop uses `read_exact` (NOT cancellation-safe), so the channel is drained by its own
registered task (`command_loop`), not a `select!` arm. Both share the `Arc<Mutex<WriteHalf>>`.

The wire verbs return `ClientActionResult::Custom { name: "peer_handshake" | "peer_message" }`,
which the generic `handle_stream_client_command` cannot execute — so `command_loop` is a
bespoke arm that routes each command through `execute_peer_action` (the same encoder the LLM
path uses), records an `injected_action` access-log entry, and replies with the outcome. An
injected `disconnect` writes nothing (BitTorrent has no graceful-close message) and
half-closes the write side; the server reads EOF and the read loop runs its normal teardown.
`remove_client_handle` is called on every read-loop exit so the rail stops offering `[ send ]`
on a dead client.

## Limitations

1. **No piece verification** - SHA1 hash checking not implemented (LLM must handle)
2. **No endgame mode** - Optimization for last pieces not automatic
3. **No rate limiting** - Upload/download rate control is LLM's responsibility
4. **No fast extension** - BEP 6 (Fast Extension) not implemented
5. **Simplified message parsing** - Complex extensions (BEP 9, BEP 10) partially supported
6. **No automatic choking** - LLM must implement choking algorithm

## Piece Selection Strategies

The LLM can implement various strategies:

1. **Rarest-First:** Request pieces that fewest peers have
2. **Sequential:** Download pieces in order (for streaming)
3. **Random-First:** Random piece selection (initial startup)
4. **Endgame:** Request same piece from multiple peers near completion

## Testing Strategy

See `tests/client/torrent_peer/CLAUDE.md` for E2E testing details.

## Example LLM Prompts

```
"Connect to peer at 192.168.1.100:6881, send handshake with info_hash abc123... and peer_id -TR2940-xyz..."

"Send interested message to peer, then request piece 0 offset 0 length 16384"

"When peer sends piece data, verify hash and request next piece using rarest-first strategy"

"Send choke message to peer and stop uploading pieces"
```

## Protocol Extensions

Common extensions (not fully implemented):

- **BEP 6:** Fast Extension (reject/allowed messages)
- **BEP 9:** Extension for Peers to Send Metadata Files
- **BEP 10:** Extension Protocol (capability negotiation)
- **BEP 11:** Peer Exchange (PEX)

## References

- [BEP 3: The BitTorrent Protocol Specification](http://www.bittorrent.org/beps/bep_0003.html)
- [BEP 10: Extension Protocol](http://www.bittorrent.org/beps/bep_0010.html)
- [Peer Wire Protocol Specification](https://wiki.theory.org/BitTorrentSpecification#Peer_wire_protocol_.28TCP.29)

## Inbound frame bound (October 2026 review)

`read_peer_message` limits the length prefix to `MAX_PEER_MESSAGE_BYTES` (8 MiB,
including the message ID), before allocating or reading a body. Zero is a keepalive;
truncated or oversized messages end the session and remove its command handle. The
bound permits ordinary block messages and large bitfields/extensions but intentionally
refuses larger extensions. CPU-only regression coverage is in
`tests/client_review_regression_test.rs::torrent_peer`.
