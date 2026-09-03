# BitTorrent DHT Protocol - Testing

## Testing Strategy

**Test Type**: Black-box E2E testing with real BitTorrent DHT clients
**Test File**: `tests/server/torrent_dht/e2e_test.rs`
**Feature Gate**: `#[cfg(all(test, feature = "torrent-dht"))]`

## Test Execution

### Prerequisites

```bash
# Build release binary
./cargo-isolated.sh build --release --no-default-features --features torrent-dht

# Install test dependencies (Ubuntu/Debian)
sudo apt-get install transmission-daemon aria2 python3-bencodepy

# Or macOS
brew install transmission aria2
pip3 install bencode.py
```

### Running Tests

```bash
# Run DHT E2E tests only
./cargo-isolated.sh test --no-default-features --features torrent-dht --test torrent_dht_e2e

# Expected output:
# - test test_dht_ping_query ... ok (5-10s)
# - test test_dht_find_node_query ... ok (5-10s)
# - test test_dht_get_peers_query ... ok (5-10s)
```

## LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Actual Breakdown**:

- **Server startup**: 1 call (parse prompt)
- **First ping**: 1 call (new event type)
- **Subsequent pings**: 0 calls (pattern reuse)
- **First find_node**: 1 call (new event type)
- **Subsequent find_node**: 0 calls
- **First get_peers**: 1 call (new event type)
- **Subsequent get_peers**: 0 calls

**Total Estimated**: 4-5 LLM calls per full suite (well under 10 target)

**Optimization**: Single server instance for all test cases.

## Runtime Expectations

**Per Test**:

- Server startup: 5-10s (LLM parses instruction)
- Per DHT query: 0.5-2s (first query slower, subsequent faster)
- UDP roundtrip: < 100ms

**Full Suite**: 15-30s with Ollama `qwen3-coder:30b` model

## Test Scenarios

### Test 1: DHT Ping Query

**Objective**: Verify basic DHT ping/pong

**Setup**:

1. Start NetGet DHT node on port {AVAILABLE_PORT}
2. Prompt: "Start a BitTorrent DHT node. Respond to ping queries with node ID 0123456789abcdef0123456789abcdef01234567."

**Test Steps**:

1. Craft ping KRPC message:
   ```python
   {
     "t": "aa",
     "y": "q",
     "q": "ping",
     "a": {"id": "abcdefghij0123456789"}  # 20 bytes
   }
   ```
2. Send via UDP to NetGet DHT node
3. Receive response
4. Verify response structure:
   ```python
   {
     "t": "aa",                           # Same transaction ID
     "y": "r",                            # Response type
     "r": {"id": "<20-byte node ID>"}
   }
   ```

**Validation**:

```rust
let response: Value = serde_bencode::from_bytes(&response_data)?;
assert!(matches!(response, Value::Dict(_)));

let dict = response.as_dict().unwrap();
assert_eq!(dict.get(b"y".as_ref()), Some(&Value::Bytes(b"r".to_vec())));
assert_eq!(dict.get(b"t".as_ref()), Some(&Value::Bytes(b"aa".to_vec())));

let r_dict = dict.get(b"r".as_ref()).unwrap().as_dict().unwrap();
let node_id = r_dict.get(b"id".as_ref()).unwrap().as_bytes().unwrap();
assert_eq!(node_id.len(), 20);
```

### Test 2: DHT Find Node Query

**Objective**: Test node routing table query

**Setup**: Same as Test 1

**Test Steps**:

1. Craft find_node query:
   ```python
   {
     "t": "bb",
     "y": "q",
     "q": "find_node",
     "a": {
       "id": "abcdefghij0123456789",
       "target": "0123456789abcdefghij"
     }
   }
   ```
2. Send to NetGet DHT node
3. Verify response:
   ```python
   {
     "t": "bb",
     "y": "r",
     "r": {
       "id": "<20-byte node ID>",
       "nodes": "<compact node info>"  # 26 bytes per node
     }
   }
   ```

**Node Validation**:

```rust
let nodes_bytes = r_dict.get(b"nodes".as_ref()).unwrap().as_bytes().unwrap();
assert_eq!(nodes_bytes.len() % 26, 0, "Nodes must be 26-byte multiples");

// Parse first node (if any)
if nodes_bytes.len() >= 26 {
    let node_id = &nodes_bytes[0..20];
    let ip = &nodes_bytes[20..24];
    let port = u16::from_be_bytes([nodes_bytes[24], nodes_bytes[25]]);

    assert_eq!(node_id.len(), 20);
    println!("Node: ID={}, IP={}.{}.{}.{}, Port={}",
        hex::encode(node_id), ip[0], ip[1], ip[2], ip[3], port);
}
```

**LLM Behavior**:

- LLM may return empty nodes list (valid if routing table is empty)
- LLM may return random nodes (acceptable for testing)
- LLM may return nodes sorted by XOR distance to target (ideal)

### Test 3: DHT Get Peers Query

**Objective**: Test peer discovery for info_hash

**Setup**:

1. Prompt: "Start a BitTorrent DHT node. For get_peers queries, return peers [192.168.1.100:6881, 192.168.1.101:6882]
   for any info_hash. Include token 'test_token'."

**Test Steps**:

1. Craft get_peers query:
   ```python
   {
     "t": "cc",
     "y": "q",
     "q": "get_peers",
     "a": {
       "id": "abcdefghij0123456789",
       "info_hash": "fedcba9876543210fedc"  # 20 bytes
     }
   }
   ```
2. Send to NetGet DHT node
3. Verify response has EITHER:
    - **Peers**: `"values": [<compact peer list>]`
    - **OR Nodes**: `"nodes": <compact node list>` (fallback)
4. Must include: `"token": "<opaque value>"`

**Peer Response Validation**:

```rust
let r_dict = dict.get(b"r".as_ref()).unwrap().as_dict().unwrap();
let token = r_dict.get(b"token".as_ref()).expect("Missing token");

if let Some(Value::Bytes(values)) = r_dict.get(b"values".as_ref()) {
    // Peer list provided
    assert_eq!(values.len() % 6, 0, "Peers must be 6-byte multiples");

    let ip = &values[0..4];
    let port = u16::from_be_bytes([values[4], values[5]]);
    println!("Peer: {}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);
} else if let Some(Value::Bytes(nodes)) = r_dict.get(b"nodes".as_ref()) {
    // Fallback to nodes
    assert_eq!(nodes.len() % 26, 0);
    println!("No peers, returned {} nodes", nodes.len() / 26);
} else {
    panic!("Response must contain either 'values' (peers) or 'nodes'");
}
```

### Test 4: Transaction ID Preservation

**Objective**: Verify response transaction IDs match query

**Test Steps**:

1. Send 3 queries with different transaction IDs: "t1", "t2", "t3"
2. Verify each response has matching transaction ID
3. Test with 2-byte binary transaction IDs (not just ASCII)

**Validation**:

```rust
let queries = vec![
    (b"t1".to_vec(), "ping"),
    (b"t2".to_vec(), "find_node"),
    (b"aa\xbb".to_vec(), "ping"),  // Binary transaction ID
];

for (tid, query_type) in queries {
    let query = build_query(&tid, query_type);
    socket.send_to(&query, dht_addr).await?;

    let (n, _) = socket.recv_from(&mut buf).await?;
    let response: Value = serde_bencode::from_bytes(&buf[..n])?;

    let dict = response.as_dict().unwrap();
    let response_tid = dict.get(b"t".as_ref()).unwrap().as_bytes().unwrap();
    assert_eq!(response_tid, &tid, "Transaction ID mismatch");
}
```

### Test 5: Invalid Query Handling

**Objective**: Test error responses for malformed queries

**Test Steps**:

1. Send query with missing "id" field
2. Send query with invalid query type
3. Send non-bencode garbage data

**Expected**:

- LLM may respond with error message: `{"y": "e", "e": [code, "message"]}`
- OR LLM may not respond at all (acceptable for UDP)

**Validation**:

```rust
// Timeout is acceptable for invalid queries
match tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buf)).await {
    Ok(Ok((n, _))) => {
        // If response received, should be error type
        let response: Value = serde_bencode::from_bytes(&buf[..n])?;
        let dict = response.as_dict().unwrap();
        assert_eq!(dict.get(b"y".as_ref()), Some(&Value::Bytes(b"e".to_vec())));
    },
    Ok(Err(e)) => panic!("Socket error: {}", e),
    Err(_) => {
        // Timeout is acceptable (no response to invalid query)
        println!("No response to invalid query (acceptable)");
    }
}
```

## Real Client Testing

### Using transmission-daemon

**Configure DHT**:

```bash
# Stop transmission daemon
sudo systemctl stop transmission-daemon

# Edit settings.json
sudo nano /var/lib/transmission-daemon/.config/transmission-daemon/settings.json

# Set DHT port and enable DHT
"dht-enabled": true,
"peer-port": 51413,

# Add NetGet as bootstrap node (not standard, but can test queries)
```

**Monitor Traffic**:

```bash
# tcpdump to capture DHT queries
sudo tcpdump -i lo -n -X 'udp port {PORT}'
```

### Using Custom Python Client

**Send DHT Query**:

```python
#!/usr/bin/env python3
import socket
import bencodepy

# Craft ping query
query = {
    b't': b'aa',
    b'y': b'q',
    b'q': b'ping',
    b'a': {b'id': b'abcdefghij0123456789'}
}

# Send to NetGet DHT node
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.sendto(bencodepy.encode(query), ('127.0.0.1', 6881))

# Receive response
data, addr = sock.recvfrom(65535)
response = bencodepy.decode(data)
print(f"Response from {addr}: {response}")
```

## Test Helpers

**Custom Helpers Needed**:

```rust
/// Build bencode KRPC query
fn build_dht_query(
    transaction_id: &[u8],
    query_type: &str,
    args: HashMap<Vec<u8>, Value>,
) -> Vec<u8> {
    let mut dict = HashMap::new();
    dict.insert(b"t".to_vec(), Value::Bytes(transaction_id.to_vec()));
    dict.insert(b"y".to_vec(), Value::Bytes(b"q".to_vec()));
    dict.insert(b"q".to_vec(), Value::Bytes(query_type.as_bytes().to_vec()));
    dict.insert(b"a".to_vec(), Value::Dict(args));
    serde_bencode::to_bytes(&Value::Dict(dict)).unwrap()
}

/// Send DHT query and receive response
async fn send_dht_query(
    socket: &UdpSocket,
    addr: SocketAddr,
    query: &[u8],
    timeout_secs: u64,
) -> Result<Value> {
    socket.send_to(query, addr).await?;

    let mut buf = vec![0u8; 65535];
    let (n, _) = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        socket.recv_from(&mut buf)
    ).await??;

    let response: Value = serde_bencode::from_bytes(&buf[..n])?;
    Ok(response)
}

/// Parse compact node info (26 bytes per node)
fn parse_compact_nodes(data: &[u8]) -> Vec<(Vec<u8>, String, u16)> {
    data.chunks(26).map(|chunk| {
        let node_id = chunk[0..20].to_vec();
        let ip = format!("{}.{}.{}.{}", chunk[20], chunk[21], chunk[22], chunk[23]);
        let port = u16::from_be_bytes([chunk[24], chunk[25]]);
        (node_id, ip, port)
    }).collect()
}

/// Parse compact peer info (6 bytes per peer)
fn parse_compact_peers(data: &[u8]) -> Vec<(String, u16)> {
    data.chunks(6).map(|chunk| {
        let ip = format!("{}.{}.{}.{}", chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        (ip, port)
    }).collect()
}
```

## Known Issues

1. **Empty Routing Table**: LLM likely won't maintain a routing table, so find_node responses may be empty or contain
   random nodes. This is expected.

2. **No Peer Storage**: LLM may not track announced peers, so get_peers may always return empty or fallback to nodes.

3. **Token Validation**: Tokens returned by get_peers are arbitrary (LLM-generated). No cryptographic validation.

4. **No Bootstrap**: DHT node doesn't join global DHT network. Isolated testing only.

5. **UDP Packet Loss**: Tests may occasionally fail due to packet loss (UDP is unreliable). Retry logic recommended.

## Debugging

**Enable TRACE logging**:

```bash
RUST_LOG=trace ./target/release/netget
```

**Manual Testing with netcat**:

```bash
# Not recommended (bencode is binary), use Python script instead
python3 dht_test.py
```

**Wireshark/tcpdump**:

```bash
# Capture DHT traffic
sudo tcpdump -i lo -n -X 'udp port 6881' -w dht_traffic.pcap

# View in Wireshark (no DHT dissector, will show as raw UDP)
```

**Bencode Inspector**:

```python
import bencodepy
with open('query.bin', 'rb') as f:
    print(bencodepy.decode(f.read()))
```

## Performance Benchmarks

**Single Query** (ping):

- Time: ~1-3s (first query with LLM)
- Time: ~0.5-1s (subsequent queries, pattern reused)

**100 Sequential Queries**:

- Time: ~30-60s (pattern reuse after first few)
- LLM calls: 3-4 (ping, find_node, get_peers × 1 each)

**Concurrent Queries**:

- Serialized through Ollama lock
- Linear slowdown with concurrency

## Success Criteria

✅ **Pass Criteria**:

- DHT node responds to ping, find_node, get_peers
- Bencode responses are valid
- Transaction IDs preserved
- Compact node/peer format correct (26/6 bytes)
- < 10 LLM calls total

❌ **Failure Indicators**:

- Bencode parse errors
- Transaction ID mismatches
- Invalid compact format lengths (not multiples of 26/6)
- Timeouts on all queries (> 10s)

## References

- [BEP 5: DHT Protocol](http://www.bittorrent.org/beps/bep_0005.html)
- [Kademlia Paper](https://pdos.csail.mit.edu/~petar/papers/maymounkov-kademlia-lncs.pdf)
- [bencode.py Documentation](https://github.com/fuzeman/bencode.py)
- [DHT Debugging with Wireshark](https://wiki.wireshark.org/BitTorrent)
