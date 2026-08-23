# BitTorrent DHT Protocol - Implementation

## Overview

The BitTorrent DHT (Distributed Hash Table) is a UDP-based peer discovery system using Kademlia DHT. It allows
decentralized peer location without centralized trackers. This implementation provides a fully LLM-controlled DHT node
that can respond to DHT queries.

## Protocol Specification

- **Base Protocol**: UDP (connectionless)
- **Encoding**: Bencode (KRPC - Kademlia RPC)
- **DHT Type**: Kademlia (160-bit node IDs, XOR distance metric)
- **Port**: Typically 6881 (user-configurable)
- **RFC/BEP**: BEP 5 (DHT Protocol)

## Architecture

### Server Implementation (`mod.rs`)

**Library Choice**: Pure Tokio + serde_bencode

- No external DHT library (KRPC parsing is straightforward)
- `tokio::net::UdpSocket` for datagram handling
- `serde_bencode` (0.2) for KRPC message encoding/decoding
- Stateless design (each query handled independently)

**Key Components**:

```rust
pub struct TorrentDhtServer;

impl TorrentDhtServer {
    pub async fn spawn_with_llm_actions(...) -> Result<SocketAddr>
    fn parse_krpc_message(data: &[u8]) -> Result<(String, serde_json::Value)>
    fn bencode_to_json(value: &serde_bencode::value::Value) -> serde_json::Value
}
```

**Connection Flow**:

1. Bind UDP socket
2. Receive datagram (up to 65535 bytes)
3. Parse bencode KRPC message
4. Identify query type (ping, find_node, get_peers, announce_peer)
5. Convert to JSON for LLM
6. LLM returns action (send_ping_response, send_find_node_response, etc.)
7. Encode response in bencode KRPC format
8. Send datagram back to peer
9. Continue listening (UDP is connectionless)

### KRPC Message Format

**Query Message**:

```python
{
  "t": "aa",                    # Transaction ID (2 bytes, arbitrary)
  "y": "q",                     # Message type: "q" = query
  "q": "ping",                  # Query method name
  "a": {                        # Arguments dictionary
    "id": "<20-byte node ID>"
  }
}
```

**Response Message**:

```python
{
  "t": "aa",                    # Same transaction ID as query
  "y": "r",                     # Message type: "r" = response
  "r": {                        # Response dictionary
    "id": "<20-byte node ID>"   # Responder's node ID
  }
}
```

**Error Message**:

```python
{
  "t": "aa",                    # Transaction ID
  "y": "e",                     # Message type: "e" = error
  "e": [201, "Error message"]   # Error code + description
}
```

### LLM Actions (`actions.rs`)

**Protocol Trait Implementation**: `Server` trait from `crate::llm::actions::protocol_trait`

Every event type calls `.with_actions(...)` and `.with_parameters(...)`. Until they did,
`call_llm` advertised none of the DHT's actions — it builds the model's tool list from
`event.event_type.actions`, not from `get_sync_actions()` — so the model was never told a
`transaction_id` existed to echo, and every DHT action it produced was rejected as unknown.

**Correlation**: KRPC's transaction id (`t`) is the correlation key. A reply that does not
echo it verbatim is discarded by the querying node, which then hangs until its own timeout.
It reaches handlers hex-encoded (the two bytes a client picks are arbitrary and usually not
text) and every reply action takes it back in the same form. A static handler can echo it
without an LLM call: `"transaction_id": "{{event.transaction_id}}"`.

**Sync Actions** (network-triggered):

1. **send_ping_response** - Respond to DHT ping
    - Parameters: `transaction_id` (hex, required), `node_id` (hex, 40 chars, optional —
      defaults to all zeros, which real nodes treat as suspicious)
    - Example:
   ```json
   {"type": "send_ping_response", "transaction_id": "{{event.transaction_id}}",
    "node_id": "0123456789abcdef0123456789abcdef01234567"}
   ```

2. **send_find_node_response** - Return closest nodes to target
    - Parameters: `transaction_id`, `node_id`, `nodes` (array of `{id, ip, port}`)
    - Output: compact node info, 26 bytes per node (20 ID + 4 IPv4 + 2 port). Entries with
      a non-20-byte id or a non-IPv4 address are silently dropped.

3. **send_get_peers_response** - Return peers for info_hash
    - Parameters: `transaction_id`, `node_id`, `token` (plain text, default `"token"`),
      `peers` (array of `{ip, port}`)
    - Output: compact peer list under `values`, 6 bytes per peer (4 IPv4 + 2 port). Omit
      `peers` entirely to signal "no peers known".

4. **send_announce_peer_response** - Acknowledge an announce_peer
    - Parameters: `transaction_id`, `node_id`
    - The BEP 5 reply body for announce_peer is just the responding node's ID, identical to
      ping's. It gets its own action name so the model is not told to answer an
      announcement with a "ping response". Nothing about the announcement is stored.

5. **send_dht_error_response** - Reject a query with a KRPC error (`y=e`)
    - Parameters: `transaction_id`, `code` (201 generic (default) / 202 server / 203
      protocol / 204 method unknown), `message`
    - Available on every event; the natural answer to a `query_type` this server does not
      support is code 204.

**Event Types** (incoming queries):

Every query event carries `transaction_id`, `query_type` and (when present) `id`.
`query_type` is the `q` method name exactly as it arrived.

1. **dht_ping_query** — node health check
2. **dht_find_node_query** — adds `target` (hex)
3. **dht_get_peers_query** — adds `info_hash` (hex)
4. **dht_announce_peer_query** — adds `info_hash` (hex), `port`, `token`

An unrecognised `q` also reaches `dht_ping_query`, whose reply shape (`{"id": ...}`) is the
KRPC minimum, but `query_type` says what actually arrived so a handler can answer with
`send_dht_error_response` code 204 instead. This is logged at WARN.

**Hex encoding of ID fields**: `id`, `target` and `info_hash` are always rendered as hex,
unconditionally. The generic `bencode_to_json` converter renders a byte string as *text*
whenever every byte happens to be printable ASCII, so the same field used to arrive
hex-encoded or not depending on the client's random node ID — and `hex::decode` on the
response side then failed. These three keys bypass that heuristic.

### Compact Encoding

**Compact Node Info** (26 bytes per node):

- 20 bytes: Node ID
- 4 bytes: IPv4 address (network byte order)
- 2 bytes: Port (network byte order)

**Compact Peer Info** (6 bytes per peer):

- 4 bytes: IPv4 address
- 2 bytes: Port

**Encoding Implementation**:

```rust
// Nodes
let mut compact = id;                                    // 20 bytes
compact.extend_from_slice(&ip_parts);                   // 4 bytes
compact.extend_from_slice(&port.to_be_bytes());         // 2 bytes

// Peers
let mut compact = ip_parts;                              // 4 bytes
compact.extend_from_slice(&port.to_be_bytes());         // 2 bytes
```

### Bencode ↔ JSON Conversion

**bencode_to_json()**: Recursively converts bencode to JSON

- `Value::Int` → `json!(i)`
- `Value::Bytes` → UTF-8 string if printable, else hex string
- `Value::List` → JSON array
- `Value::Dict` → JSON object

**Example**:

```rust
// Bencode: d1:ti2e1:y1:q1:q4:ping1:ad2:id20:abcdefghij0123456789ee
// JSON: {"t": 2, "y": "q", "q": "ping", "a": {"id": "6162636465666768696a30313233343536373839"}}
```

## LLM Integration

### Instruction Guidelines

**Example Instruction**:

```
You are a BitTorrent DHT node. Respond to ping queries with your node ID. For find_node queries, return a list of nearby nodes. For get_peers queries, return known peers for the info_hash if available, otherwise return nodes.
```

**Behavior Control**:

- **Node ID**: LLM can use a fixed node ID (e.g., all zeros, or random) or generate per-response
- **Routing Table**: LLM can maintain a routing table in conversation history or return empty/random nodes
- **Peer Storage**: LLM can track announced peers or always return empty peer lists
- **Token Generation**: LLM should generate tokens for get_peers (required for announce_peer validation)

### Typical LLM Response Flow

**Ping Query**:

1. LLM receives: `{transaction_id: "aa", id: "abcd..."}`
2. LLM returns: `{type: "send_ping_response", transaction_id: "aa", node_id: "0000..."}`

**Find Node Query**:

1. LLM receives: `{transaction_id: "bb", id: "abcd...", target: "1234..."}`
2. LLM returns closest nodes (or random nodes if no routing table):
   ```json
   {
     "type": "send_find_node_response",
     "transaction_id": "bb",
     "nodes": [
       {"id": "1111...", "ip": "192.168.1.10", "port": 6881},
       {"id": "2222...", "ip": "192.168.1.20", "port": 6881}
     ]
   }
   ```

**Get Peers Query**:

1. LLM receives: `{transaction_id: "cc", id: "abcd...", info_hash: "xyz..."}`
2. If LLM knows peers: Return peer list
3. If LLM doesn't know: Return nodes (fallback to find_node behavior)
4. LLM must include token for future announce_peer

## Logging Strategy

**DEBUG Level**:

- Datagram received (size, peer address)
- Query type identified
- LLM call initiated
- Response sent (size)

**TRACE Level**:

- Full datagram (hex)
- Parsed KRPC structure
- Full response (hex)

**INFO Level**:

- LLM-generated messages

**ERROR Level**:

- Bencode parse errors
- LLM call failures
- Socket errors

## When the LLM cannot answer

A KRPC query is request/response: the querying node holds the transaction open, retries, and
eventually marks this node bad. Silence is therefore a stall, not a "not me", so every path
that produces no reply answers with a BEP 5 error message instead:

```
{"t": <echoed transaction id>, "y": "e", "e": [<code>, "<category text>"]}
```

- **`call_llm` returned `Err`** — `WireFailure::classify` picks the code: **202** ("Server
  Error") for an overloaded backend, so the querying node retries later, **201** ("Generic
  Error") for anything else. Logged `decision=fail_closed_llm_error`.
- **The model answered with nothing that reaches the wire** — code 201, logged
  `decision=fail_closed_no_action`. Distinct from the error case in the log, identical on
  the wire, because the peer has no use for the difference.
- **The model refused explicitly** with `send_dht_error_response` — its own code and message
  go out unchanged, logged `decision=model_reject`. A successful reply logs
  `decision=model_answer`.

The peer-visible message is `WireFailure::text()`, a `&'static str`. The backend error, the
model name and any path go to `tracing::error!` and the status stream only — never into the
datagram. See `src/utils/wire_failure.rs` and `tests/server/torrent_dht/llm_failure_test.rs`.

One path stays deliberately silent: a datagram that does not parse as a KRPC **query** (bad
bencode, a `y` of `r`/`e`, a missing `t`). There is no transaction id to echo, and an
unaddressed reply is discarded by the peer anyway.

## Connection State Tracking

`protocol_info` is `ProtocolConnectionInfo::empty()`. `ProtocolConnectionInfo` is a generic
`serde_json::Value` wrapper (`state/server.rs`), not the per-protocol enum earlier versions
of this file described: there is no `TorrentDht` variant and no `recent_queries` list. UDP
is connectionless, so each datagram creates one short-lived connection entry.

## Limitations

1. **No Routing Table**: LLM-based DHT doesn't maintain a persistent Kademlia routing table. Responses may be random or
   empty.

2. **No Peer Storage**: `send_announce_peer_response` acknowledges an announcement and
   records nothing. Protocols must not implement storage; the LLM's memory (or the generic
   SQLite facility, if it opts in) is where any peer table would live.

3. **No Bootstrap**: No automatic bootstrapping to join the global DHT network. Node exists in isolation unless manually
   connected.

4. **IPv4 Only**: Compact encoding only supports IPv4. No IPv6 support (BEP 32).

5. **No Security Extensions**: BEP 42 (DHT Security Extension) not implemented. Node ID spoofing is possible.

6. **Stateless**: Each query is independent. No concept of "good" vs "bad" nodes, timeouts, or routing table
   maintenance.

7. **Limited Query Types**: ping, find_node, get_peers and announce_peer are implemented.
   Anything else falls through to `dht_ping_query` with `query_type` set to the real
   method name.

## DHT Concepts

**Node ID**: 160-bit (20-byte) identifier. Randomly chosen at startup. Should be persistent across restarts for real DHT
participation.

**XOR Distance Metric**: Distance between two node IDs is XOR of their binary representations. Closer IDs = smaller XOR
result.

**K-Buckets**: Routing table divided into 160 buckets (one per bit prefix). Each bucket stores up to K nodes (K=8
typical).

**Iterative Lookup**: To find a node/peer:

1. Query closest known nodes
2. They return even closer nodes
3. Repeat until target found or no closer nodes exist

**Token Mechanism**: get_peers returns a token. Client must echo this token in announce_peer to prove they recently
queried. Prevents announce spam.

## Security Considerations

- **No rate limiting**: Vulnerable to query floods (LLM could implement via scheduled tasks)
- **No ID verification**: Node IDs can be spoofed (BEP 42 solves this)
- **Token validation**: Tokens are generated by LLM, no cryptographic verification
- **Amplification attacks**: Small query → large response (e.g., 100 nodes = 2600 bytes)

## Testing

See `tests/server/torrent_dht/CLAUDE.md` for comprehensive testing documentation.

## References

- [BEP 5: DHT Protocol](http://www.bittorrent.org/beps/bep_0005.html)
- [BEP 32: IPv6 DHT](http://www.bittorrent.org/beps/bep_0032.html)
- [BEP 42: DHT Security Extension](http://www.bittorrent.org/beps/bep_0042.html)
- [Kademlia Paper](https://pdos.csail.mit.edu/~petar/papers/maymounkov-kademlia-lncs.pdf)
- [BitTorrent DHT Specification](https://wiki.theory.org/BitTorrentSpecification#Distributed_Hash_Table)
