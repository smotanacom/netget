# BitTorrent Tracker Protocol - Testing

## Testing Strategy

**Test Type**: Black-box E2E testing with real BitTorrent clients
**Test File**: `tests/server/torrent_tracker/e2e_test.rs`
**Feature Gate**: `#[cfg(all(test, feature = "torrent-tracker"))]`

## Test Execution

### Prerequisites

```bash
# Build release binary first (faster execution)
./cargo-isolated.sh build --release --no-default-features --features torrent-tracker

# Install test dependencies (Ubuntu/Debian)
sudo apt-get install transmission-cli aria2

# Or macOS
brew install transmission-cli aria2
```

### Running Tests

```bash
# Run tracker E2E tests only
./cargo-isolated.sh test --no-default-features --features torrent-tracker --test torrent_tracker_e2e

# Expected output:
# - test test_tracker_announce_and_scrape ... ok (20-30s)
# - test test_tracker_error_response ... ok (5-10s)
```

## LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Actual Breakdown**:

- **Server startup**: 1 call (parse prompt and initialize)
- **First announce**: 1 call (new event type)
- **Subsequent announces**: 0 calls (LLM reuses pattern)
- **First scrape**: 1 call (new event type)
- **Subsequent scrapes**: 0 calls (LLM reuses pattern)
- **Error cases**: 1 call

**Total Estimated**: 4-5 LLM calls per full suite (well under 10 target)

**Optimization**: Reuse single server instance across multiple test cases to minimize startup overhead.

## Runtime Expectations

**Per Test**:

- Server startup: 5-10s (LLM parses instruction)
- Per announce request: 0.5-2s (first request slower, subsequent faster)
- Per scrape request: 0.5-2s
- Torrent client connection: 0.1-0.5s

**Full Suite**: 20-40s with Ollama `qwen3-coder:30b` model

## Test Scenarios

### Test 1: Basic Announce and Scrape

**Objective**: Verify tracker responds correctly to announce and scrape requests

**Setup**:

1. Start NetGet tracker on port {AVAILABLE_PORT}
2. Prompt: "Start a BitTorrent tracker. Return peer lists for announce requests with 30-minute interval. For scrape
   requests, return statistics."
3. Create test torrent file with known info_hash

**Test Steps**:

1. **First Announce** (Client A):
    - Send:
      `GET /announce?info_hash=<hash>&peer_id=AAAAAAAAAAAAAAAAAAAA&port=6881&uploaded=0&downloaded=0&left=1000000&event=started`
    - Verify: Response is valid bencode with `interval` key
    - Verify: HTTP 200 status
    - Verify: `peers` list is present (may be empty)

2. **Second Announce** (Client B):
    - Send: Same request with different peer_id
    - Verify: Response includes Client A in peers list (if LLM tracks state)

3. **Scrape Request**:
    - Send: `GET /scrape?info_hash=<hash>`
    - Verify: Response is valid bencode with `files` dictionary
    - Verify: Statistics present (complete/incomplete/downloaded)

**Expected LLM Behavior**:

- LLM should track announced peers in conversation context
- Return peer lists with compact or dictionary format
- Update statistics based on announce events

**Validation**:

```rust
// Parse bencode response
let response: Value = serde_bencode::from_bytes(&body)?;
assert!(matches!(response, Value::Dict(_)));

// Check interval
let dict = response.as_dict().unwrap();
let interval = dict.get(b"interval".as_ref())
    .and_then(|v| v.as_int())
    .expect("Missing interval");
assert!(interval > 0);

// Check peers
assert!(dict.contains_key(b"peers".as_ref()));
```

### Test 2: Announce Events

**Objective**: Test different announce event types

**Test Cases**:

- **event=started**: Client joins swarm
- **event=completed**: Client finishes download (becomes seeder)
- **event=stopped**: Client leaves swarm

**Verification**:

- LLM should track peer state changes
- Scrape statistics should update (complete/incomplete counts)

### Test 3: Compact vs Non-Compact Responses

**Objective**: Verify both peer list formats

**Test Steps**:

1. Announce with `compact=1`:
    - Verify: `peers` value is a byte string (Value::Bytes)
    - Verify: Length is multiple of 6 (4 bytes IP + 2 bytes port)

2. Announce with `compact=0` or no compact param:
    - Verify: `peers` value is a list (Value::List)
    - Verify: Each peer is a dictionary with peer_id, ip, port

### Test 4: Error Responses

**Objective**: Test tracker error handling

**Test Steps**:

1. Send malformed request (missing required params)
2. Verify: Response contains `failure reason` key
3. Verify: HTTP 200 status (errors still return 200 with bencode error)

**LLM Prompt Addition**: "Return an error with send_error_response if required parameters are missing."

### Test 5: Multi-Torrent Scrape

**Objective**: Scrape multiple torrents in one request

**Test Steps**:

1. Announce to 3 different torrents
2. Scrape all 3: `GET /scrape?info_hash=<hash1>&info_hash=<hash2>&info_hash=<hash3>`
3. Verify: Response contains all 3 info_hashes in `files` dictionary

## Real Client Testing

### Using transmission-cli

**Create Test Torrent**:

```bash
# Create dummy file
dd if=/dev/zero of=test.dat bs=1M count=10

# Create torrent with NetGet tracker
transmission-create -o test.torrent -t http://127.0.0.1:{PORT}/announce test.dat

# Start seeder
transmission-cli test.torrent
```

**Verify**:

- Check NetGet logs for announce request
- Verify bencode response parsing
- Check for periodic re-announce (every interval seconds)

### Using aria2

**Download with NetGet tracker**:

```bash
aria2c --bt-tracker=http://127.0.0.1:{PORT}/announce test.torrent
```

**Verify**:

- aria2 shows peer list from tracker
- Multiple announces visible in logs
- Scrape requests if aria2 sends them

## Test Helpers

**Available Helpers** (from `tests/server/helpers.rs`):

- `start_server_with_instruction()`: Start NetGet with custom prompt
- `wait_for_server_ready()`: Wait for server to bind port
- Port allocation: Use `{AVAILABLE_PORT}` placeholder in prompts

**Custom Helpers Needed**:

```rust
/// Create a minimal .torrent file for testing
fn create_test_torrent(tracker_url: &str, piece_length: usize, total_size: usize) -> Vec<u8> {
    // Build bencode torrent dictionary
    // Return .torrent file bytes
}

/// Send tracker announce request
async fn send_announce(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &str,
    port: u16,
    event: Option<&str>,
) -> Result<Value> {
    // Build URL with parameters
    // Send HTTP GET
    // Parse bencode response
}

/// Send tracker scrape request
async fn send_scrape(tracker_url: &str, info_hashes: &[&[u8; 20]]) -> Result<Value> {
    // Build scrape URL
    // Send HTTP GET
    // Parse bencode response
}
```

## Known Issues

1. **LLM State Tracking**: LLM may not consistently track peers across multiple announces. This is expected behavior (
   stateless tracker) unless LLM explicitly uses conversation history.

2. **Peer List Format**: LLM may default to non-compact format even when compact=1 requested. This is acceptable (
   clients support both formats).

3. **Statistics Accuracy**: Scrape statistics may be estimates rather than exact counts (depends on LLM's internal
   tracking).

4. **IPv6**: Not supported in compact format. Test only IPv4 scenarios.

## Debugging

**Enable TRACE logging**:

```bash
RUST_LOG=trace ./target/release/netget
# Press Ctrl+L in TUI to cycle log levels
```

**Manual Testing**:

```bash
# Start NetGet tracker
./target/release/netget
> start a bittorrent tracker on port 6969

# In another terminal, send announce
curl -v "http://127.0.0.1:6969/announce?info_hash=%01%23%45%67%89%AB%CD%EF%01%23%45%67%89%AB%CD%EF%01%23%45%67&peer_id=TESTPEERID12345678&port=6881&uploaded=0&downloaded=0&left=1000000&event=started"

# Send scrape
curl -v "http://127.0.0.1:6969/scrape?info_hash=%01%23%45%67%89%AB%CD%EF%01%23%45%67%89%AB%CD%EF%01%23%45%67"
```

**Bencode Inspection**:

```python
# Python helper to decode responses
import bencodepy
response = b"d8:intervali1800e5:peers0:e"  # From curl
print(bencodepy.decode(response))
# Output: {b'interval': 1800, b'peers': b''}
```

## Performance Benchmarks

**Single Client** (1 announce + 1 scrape):

- Time: ~3-5s
- LLM calls: 2 (announce + scrape)

**10 Sequential Clients** (10 announces + 1 scrape):

- Time: ~10-15s
- LLM calls: 2-3 (first announce, first scrape, pattern reuse after)

**Concurrent Clients**:

- Requests serialized to prevent Ollama overload
- Expected slowdown proportional to client count

## Success Criteria

✅ **Pass Criteria**:

- All tests pass with real BitTorrent clients
- LLM responds to both announce and scrape requests
- Bencode encoding is valid
- HTTP responses are well-formed
- < 10 LLM calls total

❌ **Failure Indicators**:

- Bencode parse errors in client
- Missing required dictionary keys (interval, peers, files)
- HTTP errors (500, 404)
- Timeouts (> 30s per request)

## References

- [BEP 3: Tracker Protocol](http://www.bittorrent.org/beps/bep_0003.html)
- [transmission-cli Documentation](https://transmissionbt.com/help/gtk/2.00/html/)
- [aria2 BitTorrent Options](https://aria2.github.io/manual/en/html/aria2c.html#bittorrent-options)
