# BitTorrent DHT Client Implementation

## Overview

The BitTorrent DHT (Distributed Hash Table) client provides LLM-controlled access to the BitTorrent Kademlia-based
distributed peer discovery network. Unlike trackers, DHT is decentralized and doesn't rely on central servers.

## Protocol Details

**Protocol:** BitTorrent DHT Protocol (BEP 5)
**Transport:** UDP with bencode-encoded messages
**Port:** Typically 6881 (standard BitTorrent DHT port)
**Stack:** ETH > IP > UDP > BitTorrent-DHT

## Implementation

### Library Choices

- **UDP Socket:** `tokio::net::UdpSocket` - Async UDP sockets
- **Bencode:** `serde_bencode` - Serialization/deserialization of DHT messages

### Architecture

1. **Connectionless:** UDP-based, no persistent connections
2. **Query-Response:** Request-response pattern with transaction IDs
3. **Kademlia DHT:** 160-bit node IDs, XOR distance metric
4. **LLM Integration:** LLM controls DHT queries and interprets responses

### Connection Flow

```
1. UDP socket bound to local port
2. LLM triggers DHT query (ping, find_node, get_peers, announce_peer)
3. Query sent to remote DHT node
4. Response received and parsed
5. LLM analyzes response (nodes, peers, errors)
6. LLM decides: follow-up queries, connect to peers, or stop
```

### Message Format

**Query Message (bencode):**

```
d1:t2:aa1:y1:q1:q9:find_node1:ad2:id20:<node_id>6:target20:<target_id>ee
```

**Response Message (bencode):**

```
d1:t2:aa1:y1:r1:rd2:id20:<node_id>5:nodes<compact node info>ee
```

**Error Message (bencode):**

```
d1:t2:aa1:y1:e1:eli201e23:Generic Error Messageee
```

### DHT Query Types

1. **ping** - Check if node is alive
2. **find_node** - Find nodes close to target ID
3. **get_peers** - Find peers for info_hash
4. **announce_peer** - Announce that we have a torrent

## LLM Control Points

### Actions

1. **dht_ping** - Ping a DHT node
    - Parameters: node_id, transaction_id
    - LLM decides: which nodes to ping

2. **dht_find_node** - Find nodes close to target
    - Parameters: node_id, target, transaction_id
    - LLM decides: target ID, iterative routing strategy

3. **dht_get_peers** - Get peers for info_hash
    - Parameters: node_id, info_hash, transaction_id
    - LLM decides: which info_hash to query

4. **dht_announce_peer** - Announce torrent availability
    - Parameters: node_id, info_hash, transaction_id
    - LLM decides: when and what to announce

5. **disconnect** - Stop DHT client

### Events

1. **dht_response** - Received response from DHT node
    - Data: message_type (q/r/e), query_type, response, error, peer address
    - LLM analyzes: node lists, peer lists, routing table updates

## Limitations

1. **No routing table management** - LLM must manually track nodes (no automatic K-bucket management)
2. **No automatic node refresh** - LLM must explicitly re-query nodes
3. **Simplified Kademlia** - No automatic iterative lookups
4. **Single query mode** - No parallel queries to multiple nodes
5. **No token management** - announce_peer requires tokens from get_peers (not implemented)

## Testing Strategy

See `tests/client/torrent_dht/CLAUDE.md` for E2E testing details.

## Example LLM Prompts

```
"Connect to DHT node at router.bittorrent.com:6881 and ping with node_id abc123..."

"Send find_node query to find nodes close to target xyz789..."

"Query DHT for peers with info_hash abc123... and connect to returned peers"

"Announce that I'm seeding info_hash xyz789... to the DHT"
```

## DHT Bootstrap Nodes

Common bootstrap nodes for joining the DHT network:

- `router.bittorrent.com:6881`
- `router.utorrent.com:6881`
- `dht.transmissionbt.com:6881`

## References

- [BEP 5: DHT Protocol](http://www.bittorrent.org/beps/bep_0005.html)
- [Kademlia: A Peer-to-peer Information System](https://pdos.csail.mit.edu/~petar/papers/maymounkov-kademlia-lncs.pdf)
- [DHT Protocol Specification](https://wiki.theory.org/BitTorrentSpecification#Distributed_Hash_Table)

## Injected commands (the dashboard's `[ send ]`)

The client registers a command channel (`command_support::register_command_channel`) as soon
as its UDP socket exists — **before** the connected-event LLM call, which a manual `*` routing
rule can park for minutes. Commands are drained by their own registered task rather than a
`tokio::select!` arm: `recv_from` is cancellation-safe, but the receive loop awaits
`call_llm_for_client` inline, so a `select!` arm there would stall for a whole LLM round-trip.

Injected actions go through `TorrentDhtClient::apply_action`, the same function the LLM path
uses, so the bencode encoding exists exactly once. Outcomes:

| Injected action | `ClientSendOutcome` |
|---|---|
| `dht_ping` / `dht_find_node` / `dht_get_peers` / `dht_announce_peer` / `dht_query` | `Sent { bytes_sent }` — the byte count `send_to` returned |
| `disconnect` | `Disconnected` — aborts the receive loop, drops the handle, marks the client Disconnected. UDP has no wire close, so that is what "disconnected" can mean here |
| unknown action / bad parameters | `Rejected { error }` |
| anything that produces no datagram | `Executed { detail }` |

Covered by `tests/client/torrent_dht/command_channel_test.rs`, which asserts the datagram
really arrives at a stand-in node and that `bytes_sent` equals what reached the wire.
