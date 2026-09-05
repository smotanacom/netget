# Bitcoin P2P Protocol Implementation

## Overview

This implementation provides a Bitcoin P2P protocol server that is **NOT a real full node**. Instead, the LLM controls
all protocol responses, allowing NetGet to simulate Bitcoin P2P protocol behavior without implementing the full
blockchain validation, storage, and consensus logic.

## Library Choice

**Server**: `rust-bitcoin` crate (v0.32+)

- Industry-standard Bitcoin library for Rust
- Comprehensive message parsing/serialization via `bitcoin::network::message`
- Supports all Bitcoin P2P message types (version, verack, ping, pong, getdata, inv, block, tx, etc.)
- Handles magic bytes for different networks (mainnet, testnet, signet, regtest)
- Mature, well-maintained, used in production Bitcoin software

**Client** (for testing): `rust-bitcoin` or simple TCP client sending raw P2P messages

## Architecture

### Message Flow

1. **Connection Established**: TCP connection accepted
    - Event: `bitcoin_connection_opened`
    - LLM decides: wait for peer's version, or send our version first
    - Actions available: `send_version`, `close_this_connection`

2. **Message Received**: Raw bytes arrive
    - Parse using `RawNetworkMessage::consensus_decode()`
    - Validate magic bytes match configured network
    - Extract message type and payload
    - Event: `bitcoin_message_received` with parsed message data
    - LLM decides response based on message type
    - Actions available: `send_version`, `send_verack`, `send_ping`, `send_pong`, `send_getaddr`,
      `send_bitcoin_message`, `close_this_connection`

3. **Message Sent**: LLM action executed
    - Build message using `RawNetworkMessage::new(magic, payload)`
    - Encode using `consensus_encode()`
    - Send raw bytes over TCP

### Network Support

Supports all Bitcoin networks via `network` startup parameter:

- `mainnet` (default): Magic bytes `0xF9BEB4D9`
- `testnet`: Magic bytes `0x0B110907`
- `signet`: Magic bytes `0x40CF030A`
- `regtest`: Magic bytes `0xFABFB5DA`

### Message Types Handled

The implementation parses all Bitcoin P2P message types:

- **Handshake**: `version`, `verack`
- **Connectivity**: `ping`, `pong`, `getaddr`, `addr`, `addrv2`, `sendaddrv2`
- **Inventory**: `inv`, `getdata`, `notfound`
- **Blocks**: `block`, `getblocks`, `getheaders`, `headers`, `merkleblock`
- **Transactions**: `tx`, `mempool`
- **Filtering**: `filterload`, `filteradd`, `filterclear`
- **Compact Blocks**: `sendcmpct`, `cmpctblock`, `getblocktxn`, `blocktxn`
- **BIP157**: `getcfilters`, `cfilter`, `getcfheaders`, `cfheaders`, `getcfcheckpt`, `cfcheckpt`
- **Other**: `sendheaders`, `feefilter`, `wtxidrelay`, `alert`, `reject`
- **Unknown**: Any unrecognized command

### LLM Integration

#### Sync Actions (Network Events)

Triggered by network events, return `ActionResult`:

1. **send_bitcoin_message** - ESCAPE HATCH, raw hex-encoded message
    - The only action that takes bytes, which the project's action design rules say to avoid:
      the model must produce the 24-byte header, the magic and the payload checksum itself,
      none of which are computed for it. Prefer a dedicated action; reach for this only for a
      message type that has none, and expect the peer to reject a miscomputed checksum.
    - Example: `{"type": "send_bitcoin_message", "hex_data": "f9beb4d9..."}`

2. **send_version** - Send version handshake message
    - Parameters: `network`, `version`, `services`, `user_agent`, `start_height`, `relay`
    - Automatically generates nonce and timestamp
    - Example: `{"type": "send_version", "network": "mainnet", "version": 70015}`

3. **send_verack** - Acknowledge version (complete handshake)
    - Parameters: `network`
    - Example: `{"type": "send_verack", "network": "mainnet"}`

4. **send_ping** - Send ping with nonce
    - Parameters: `network`, `nonce` (optional, random if not provided)
    - Example: `{"type": "send_ping", "nonce": 123456789}`

5. **send_pong** - Respond to ping
    - Parameters: `network`, `nonce` (must match ping nonce)
    - Example: `{"type": "send_pong", "nonce": 123456789}`

6. **send_getaddr** - Request peer addresses
    - Parameters: `network`
    - Example: `{"type": "send_getaddr", "network": "mainnet"}`

7. **close_this_connection** - Close connection
    - No parameters
    - Example: `{"type": "close_this_connection"}`

#### Event Data Format

**bitcoin_connection_opened**:

```json
{}
```

**bitcoin_message_received**:

```json
{
  "message_type": "version",
  "message": {
    "version": 70015,
    "services": 0,
    "timestamp": 1234567890,
    "receiver": "0.0.0.0:0",
    "sender": "0.0.0.0:0",
    "nonce": 1234567890,
    "user_agent": "/Satoshi:0.20.0/",
    "start_height": 700000,
    "relay": true
  }
}
```

For `ping`/`pong`:

```json
{
  "message_type": "ping",
  "message": {"nonce": 123456789}
}
```

For other message types, basic info is provided (full parsing can be added as needed).

## Logging Strategy

### Dual Logging (REQUIRED)

All logs use **both** tracing macros AND `status_tx.send()`:

```rust
debug!("Bitcoin P2P received {} bytes on {}", n, connection_id);
let _ = status_tx.send(format!("[DEBUG] Bitcoin P2P received {} bytes on {}", n, connection_id));
```

### Log Levels

- **ERROR**: Parse failures, send failures, unexpected errors
- **WARN**: Magic byte mismatches, malformed messages
- **INFO**: Connection lifecycle (accepted, closed), message type received/sent
- **DEBUG**: Byte counts, message summaries
- **TRACE**: Full hex payloads (sent and received)

### Binary Data Logging

Bitcoin P2P is a **binary protocol** - all data logged as hex:

```rust
let hex_str = hex::encode(data);
trace!("Bitcoin P2P data (hex): {}", hex_str);
let _ = status_tx.send(format!("[TRACE] Bitcoin P2P data (hex): {}", hex_str));
```

## Connection State Machine

Similar to TCP protocol, uses state machine to prevent concurrent LLM calls:

- **Idle**: No LLM processing, ready for new messages
- **Processing**: LLM currently processing message
- **Accumulating**: Waiting for more bytes to complete message

When in `Processing` state, incoming data is queued. After LLM response, queued data is processed.

## Limitations

### What This Implementation Does

- ✅ Parses all Bitcoin P2P message types
- ✅ Validates magic bytes and message format
- ✅ Provides LLM with parsed message data
- ✅ Sends properly formatted Bitcoin P2P messages
- ✅ Handles version/verack handshake
- ✅ Responds to ping/pong
- ✅ Supports all Bitcoin networks (mainnet, testnet, signet, regtest)

### What This Implementation Does NOT Do

- ❌ **Not a real full node** - does not validate blocks, transactions, or maintain blockchain state
- ❌ **No blockchain storage** - does not store blocks or implement a UTXO set
- ❌ **No consensus validation** - does not verify proof-of-work or signatures
- ❌ **No transaction relay** - does not propagate transactions to other peers
- ❌ **No mempool** - does not maintain pending transactions
- ❌ **No wallet functionality** - cannot create or sign transactions

This is a **protocol honeypot/simulator** where the LLM controls all responses. Useful for:

- Security research and honeypots
- Protocol testing and fuzzing
- Educational demonstrations
- Custom Bitcoin P2P integrations

## Example LLM Prompts

### Basic Handshake

```
Open Bitcoin P2P server on port 8333.
When a peer connects, wait for their version message.
Respond with our own version (protocol 70015, no services).
After receiving verack, complete handshake with our verack.
Handle ping/pong messages normally.
```

### Testnet Node Simulation

```
Run Bitcoin P2P server on port 18333 for testnet network.
Respond to version with version 70015, services=0.
Complete handshake, then respond to getaddr with empty addr list.
Log all message types received.
```

### Custom Behavior

```
Bitcoin P2P server on port 9333.
After handshake, ignore all getdata requests (don't respond).
Respond to ping with pong.
Disconnect after 10 messages.
```

## Dependencies

```toml
bitcoin = { version = "0.32", optional = true }
rand = "0.8"  # For generating nonces
hex = "0.4"   # For hex encoding/decoding
```

## Implementation Notes

1. **Message Parsing**: `try_parse_bitcoin_message` validates the 24-byte header itself -
   magic, then the declared payload length against a 4 MB cap - before handing exactly
   `24 + length` bytes to `RawNetworkMessage::consensus_decode()`. It reports "incomplete"
   only for a valid header whose body has not fully arrived, so the outstanding byte count is
   bounded; everything else is an error that drops the connection.

   This matters because the previous version mapped *every* decode failure to "need more
   data". A peer sending garbage was buffered indefinitely and the whole buffer re-parsed on
   each read - unbounded memory, quadratic work. Two related defects lived alongside it: the
   parse loop discarded the "remaining" slice, so a pipelined second message made the loop
   re-parse the first one and call the LLM on it forever; and the merge step did
   `conns.get_mut(&connection_id).unwrap()` after having released the lock, panicking the task
   whenever a peer disconnected while its data was in flight.

   Payload allocation itself is safe: rust-bitcoin caps vectors at `MAX_VEC_SIZE` (4 MB), so
   a bogus length prefix cannot provoke a huge allocation the way it could in hand-rolled
   parsers.

2. **Magic Bytes**: Network is configured at server startup via `network` parameter. All messages must match the
   configured network's magic bytes.

3. **Nonce Generation**: Version and ping messages use `rand::random()` for nonces.

4. **Address Fields**: Version message receiver/sender addresses are set to `0.0.0.0:0` (can be customized via LLM if
   needed).

5. **Message Encoding**: All response messages are built using `RawNetworkMessage::new()` and encoded with
   `consensus_encode()` to ensure proper format.

6. **Connection Tracking**: `ProtocolConnectionInfo` is a generic `serde_json::Value` wrapper,
   not an enum with a `Bitcoin` variant. Connections are registered with
   `ProtocolConnectionInfo::empty()` and the last message type is set via
   `update_bitcoin_connection_info`. The `handshake_complete` field on `ConnectionData` is
   written once at construction and never read.

7. **Dashboard injection (peer handle + counters)**: every accepted connection registers a
   peer handle (`server::peer_support`) right after `add_connection_to_server`, before the
   opened event, so `[ message this peer ]` / `[ disconnect this peer ]` work even while a
   manual rule parks that event. The generic peer task runs injected actions through the
   protocol's own `execute_action`; every wire verb (`send_version`, `send_verack`,
   `send_ping`, `send_pong`, `send_getaddr`, `send_bitcoin_message`) returns
   `ActionResult::Output` and `close_this_connection` returns `CloseConnection`, so nothing
   is `Custom` and the whole vocabulary is injectable. `execute_action` also accepts the
   generic `close_connection` name (not offered to the model) because that is what
   `[ disconnect this peer ]` injects. The handle is removed on every exit
   path (EOF, read error, model-driven close) via `teardown_connection`, and
   `update_connection_stats` runs on each read and each write so the rail's `↓ ↑` counters
   and `last_activity` move. A model-driven `close_this_connection` now also half-closes the
   socket (it previously only dropped the map entry and left the peer connected).
   Zero-LLM test: `tests/server/bitcoin/peer_inject_test.rs`.

8. **LLM failure is a disconnect, never a message.** When `call_llm` returns `Err` - backend
   down, rate-limit queue exhausted, no usable answer after retries - `fail_closed`
   (`mod.rs`) half-closes the write side so the peer reads EOF immediately, the same shape
   `tcp` uses. Both LLM-error sites (the `bitcoin_connection_opened` handler and the
   `bitcoin_message_received` handler) go through it; both previously logged a warning, reset
   the connection to `Idle` and wrote nothing, so a peer that had just sent `version` or
   `ping` blocked until its own timeout.

   Bitcoin P2P has no error frame worth sending: BIP61 `reject` was removed from Bitcoin Core
   in 0.20 and is ignored across the network, and it could not express "backend busy" anyway.
   Disconnecting is what a real node does when it cannot serve a peer, and it is the only
   signal that actually reaches one.

   **Nothing derived from the error goes on the wire** - FIN carries no text, and the error
   is logged and pushed to the status stream instead (see `src/utils/wire_failure.rs` and
   `tests/wire_failure_test.rs`). Because the wire has only this one shape, the
   `WireFailure::Overloaded` / `Unavailable` distinction is carried in the log:

   - `decision=fail_closed_llm_overloaded` - backend at capacity (retryable)
   - `decision=fail_closed_llm_error` - anything else the LLM call raised
   - `decision=model_no_action` - the model answered, choosing to send nothing. Legitimate
     and deliberately silent (an `addr` or an `inv` needs no reply), and the connection stays
     open.
   - `decision=model_close` - the model asked for `close_this_connection`.

   The three are distinct on purpose: a model that declines to answer must never be logged
   the same way as a backend that fell over.

## Future Enhancements

Potential additions (not currently implemented):

- Parse and provide more detail for complex message types (block, tx, inv)
- Support BIP324 encrypted P2P messages
- Implement simple blockchain state for more realistic responses
- Add async actions for broadcasting to multiple connections
- Support for addr/addrv2 message construction
- Inventory message helpers (inv, getdata, notfound)
