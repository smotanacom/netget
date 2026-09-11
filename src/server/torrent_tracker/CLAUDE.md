# BitTorrent Tracker Protocol - Implementation

## Overview

The BitTorrent Tracker is an HTTP-based coordination server that helps BitTorrent clients find peers for specific
torrents. This implementation provides a fully LLM-controlled tracker that can respond to announce and scrape requests.

## Protocol Specification

- **Base Protocol**: HTTP/1.1 over TCP
- **Encoding**: Bencode (BitTorrent's serialization format)
- **Request Types**: GET requests with query parameters
- **Response Types**: Bencode dictionaries or plaintext errors
- **Port**: Typically 6969 or 8080 (user-configurable)
- **RFC/BEP**: BEP 3 (The BitTorrent Protocol Specification)

## Architecture

### Server Implementation (`mod.rs`)

**Library Choice**: Pure Tokio implementation

- No external tracker library (HTTP parsing + bencode is sufficient)
- `tokio::net::TcpListener` for TCP connections
- `serde_bencode` (0.2) for bencode encoding/decoding
- `urlencoding` for URL-encoded query parameters

**Key Components**:

```rust
pub struct TorrentTrackerServer;

impl TorrentTrackerServer {
    pub async fn spawn_with_llm_actions(...) -> Result<SocketAddr>
    async fn handle_connection(...) -> Result<()>
    fn parse_http_request(request: &str) -> Result<(String, HashMap<String, serde_json::Value>)>
}
```

**Connection Flow**:

1. Accept TCP connection
2. Read HTTP GET request (up to 8192 bytes)
3. Parse request line and query parameters
4. Identify request type (announce or scrape)
5. Convert to JSON for LLM
6. LLM returns action (send_announce_response, send_scrape_response, or send_error_response)
7. Encode response in bencode format
8. Wrap in HTTP response
9. Send to client
10. Close connection (HTTP/1.0 style)

### LLM Actions (`actions.rs`)

**Protocol Trait Implementation**: `Server` trait from `crate::llm::actions::protocol_trait`

Both event types call `.with_actions(...)` and `.with_parameters(...)`. Until they did,
`call_llm` advertised none of the tracker's actions (it builds the model's tool list from
`event.event_type.actions`, not from `get_sync_actions()`), so every action the model
returned was rejected as unknown.

**Sync Actions** (network-triggered):

1. **send_announce_response** - Return peer list for a torrent
    - Parameters: `interval` (default 1800), `complete`, `incomplete`, `compact`, `peers`
    - `peers` is an array of `{ip, port}` plus an optional `peer_id` (used only in the
      non-compact form)
    - **`compact`**: BEP 23. When truthy, `peers` is encoded as a byte string of 6-byte
      entries (4-byte IPv4 + 2-byte big-endian port) instead of a list of dictionaries.
      Nearly every real client asks for `compact=1` and several refuse the dictionary
      form, so send `1` unless you have a reason not to.
      Accepts `true`, `1`, `"1"`, `"true"`, `"yes"`, `"on"`. IPv6 peers are dropped in
      compact form (they need BEP 7's separate `peers6` key, which is not implemented).
    - **`{{event.compact}}` works in a static handler and NOWHERE ELSE.** `{{...}}` is
      interpolated by the static-handler path only. On the LLM path the placeholder arrives
      as literal text, `is_compact` matches none of its accepted spellings, and the tracker
      answers in the dictionary form real clients refuse — with no error anywhere, which is
      how it went unnoticed. This file, the action's `example` and the event's
      `response_example` all recommended it to the model.
    - **`peer_id`** in a peer entry is decoded from hex when it is 40 hex characters, which
      is the form the announce *event* reports an inbound peer_id in. Anything else is sent
      as literal text. Echoing the event's value used to put 40 bytes on the wire where
      BEP 3 wants 20.
    - Output: HTTP 200 + `Connection: close` + bencode body
    - Example:
   ```json
   {
     "type": "send_announce_response",
     "interval": 1800,
     "complete": 10,
     "incomplete": 5,
     "compact": 1,
     "peers": [{"ip": "192.168.1.100", "port": 51413}]
   }
   ```

2. **send_scrape_response** - Return statistics for torrents
    - Parameter: `files`, accepted in **either** shape:
      - object keyed by hex info_hash: `{"<hex>": {complete, downloaded, incomplete}}`
      - array of objects each carrying an `info_hash` field
      The executor previously accepted only the object form, so the array form documented
      here (and used by the E2E test) silently produced an empty `files` dictionary.
      Entries whose key is not valid hex are dropped.
    - The info_hash is the *correlation*: it must be the one the client asked about, as
      the event's own 40-character hex. **Do not use `{{event.info_hash}}` as an object
      key on the LLM path** — it is not interpolated there, `hex::decode` rejects it, the
      entry is dropped, and the client gets an empty `files` dictionary under a 200 OK. The
      array form avoids the whole question by putting the hash in a value.
    - Example:
   ```json
   {
     "type": "send_scrape_response",
     "files": [{
       "info_hash": "1111111111111111111111111111111111111111",
       "complete": 10, "downloaded": 100, "incomplete": 5
     }]
   }
   ```

3. **send_error_response** - Return a bencode `failure reason`
    - Parameter: `failure_reason` (alias `error`), and genuinely **required**. The action
      definition, these docs and the E2E test all said `failure_reason` while the executor
      read `error`, so every documented use produced the literal string "Unknown error".
      Both spellings now work, and omitting both is now an error rather than a refusal
      that tells the client nothing — the parameter was declared `required: true` while
      being defaulted anyway.
    - Example:
   ```json
   {"type": "send_error_response", "failure_reason": "Torrent not registered"}
   ```

**Event Types** (incoming requests):

Both events always carry `request_type`, `path` and `compact`, whether or not the client
sent them. That matters for static handlers: `{{event.compact}}` is a *hard error* if the
field is missing, and plenty of clients omit `compact` from the query string.

1. **tracker_announce_request** - Client announces presence and requests peers
    - Payload: `request_type`, `path`, `compact`, `info_hash`, `peer_id`, `port`,
      `uploaded`, `downloaded`, `left`, `event`, `numwant`
    - Actions: `send_announce_response`, `send_error_response`

2. **tracker_scrape_request** - Client requests statistics
    - Payload: `request_type`, `path`, `compact`, `info_hash`
    - Actions: `send_scrape_response`, `send_error_response`

A path that is neither `/announce` nor `/scrape` also reaches
`tracker_announce_request` — a tracker has no third reply shape — but `request_type` is
`"unknown"` and `path` carries the original, so a handler can answer with
`send_error_response` instead.

### Request Parsing

**URL Format**:

```
GET /announce?info_hash=%XX%XX...&peer_id=%XX%XX...&port=6881&uploaded=0&downloaded=0&left=1000000&event=started HTTP/1.1
```

**Parameter Handling**:

- **info_hash** / **peer_id**: URL-encoded binary → hex string (40 chars)
- **port** / **uploaded** / **downloaded** / **left** / **numwant** / **compact**: Parsed as u64
- **event**: String ("started", "completed", "stopped", or empty)
- **ip**: String (client IP, optional)

**Special Cases**:

- Info hash and peer ID are 20 bytes each, URL-encoded in request
- Compact format (compact=1) returns 6-byte peer format (4 bytes IP + 2 bytes port)
- Non-compact format returns bencode list of peer dictionaries

### Response Encoding

**Announce Response (Success)**:

```python
{
  "interval": 1800,           # Seconds until next announce
  "complete": 10,             # Number of seeders (optional)
  "incomplete": 5,            # Number of leechers (optional)
  "peers": [                  # Non-compact format
    {
      "peer id": "...",       # 20 bytes
      "ip": "192.168.1.100",
      "port": 6881
    }
  ]
  # OR
  "peers": "..."              # Compact format: 6 bytes per peer (4 IP + 2 port)
}
```

**Scrape Response**:

```python
{
  "files": {
    "<20-byte info_hash>": {
      "complete": 10,
      "incomplete": 5,
      "downloaded": 100
    }
  }
}
```

**Error Response**:

```python
{
  "failure reason": "Error message"
}
```

## LLM Integration

### Instruction Guidelines

**Example Instruction**:

```
You are a BitTorrent tracker server. Track active peers for torrents and return peer lists on announce requests. Use a 30-minute announce interval. For scrape requests, return current statistics.
```

**Behavior Control**:

- **Public Tracker**: Return all known peers for any info_hash
- **Private Tracker**: Check authorization, return errors for unknown torrents
- **Peer Limits**: Control how many peers to return (numwant parameter)
- **Statistics**: Track seeders (left=0) vs leechers (left>0)

### Typical LLM Response Flow

**Announce Request**:

1. LLM receives JSON:
   `{info_hash: "abc...", peer_id: "xyz...", port: 6881, uploaded: 0, downloaded: 0, left: 1000000, event: "started"}`
2. LLM tracks this peer internally (can use schedule_task for cleanup)
3. LLM returns: `{type: "send_announce_response", interval: 1800, peers: [...]}`

**Scrape Request**:

1. LLM receives JSON: `{info_hash: "abc...", request_type: "scrape", path: "/scrape?..."}`
   (a single `info_hash`, not `info_hashes` — that field does not exist)
2. LLM looks up statistics for each torrent
3. LLM returns:
   `{type: "send_scrape_response", files: [{info_hash: "abc...", complete: 10, incomplete: 5, downloaded: 100}]}`

## Logging Strategy

**DEBUG Level**:

- Connection accepted/closed
- Request type identified
- LLM call initiated
- Response size sent

**TRACE Level**:

- Full HTTP request text
- Full HTTP response text
- Parsed query parameters

**INFO Level**:

- LLM-generated messages (via execution_result.messages)

**ERROR Level**:

- Accept errors
- Parse errors
- LLM call failures

## Connection State Tracking

`protocol_info` is `ProtocolConnectionInfo::empty()`. `ProtocolConnectionInfo` is a
generic `serde_json::Value` wrapper (`state/server.rs`), not the per-protocol enum earlier
versions of this file described; there is no `TorrentTracker` variant and no
`recent_requests` list. Each announce or scrape is a separate short-lived connection.

### Dashboard injection — stats yes, peer handle intentionally no

`handle_connection` now calls `AppState::update_connection_stats` on the single read and on
every write path (LLM response, `400 Bad Request`, `500`), so the dashboard rail shows real
`↓ ↑` byte counts and a fresh `last_activity` instead of `↓0 ↑0`.

It does **not** register a `peer_support` handle, so the rail offers no
"message this peer" / "disconnect this peer" affordance. That is deliberate: the tracker is
HTTP-style one-shot — one read, one write, then the connection returns and closes — so there
is no live window in which an injected `send_announce_response` or `close_connection` could
reach the peer. Wiring a handle would register it and immediately drop it; the honest
rendering is the dim "cannot message a peer from here yet" row. (Because there is no peer
handle, `execute_action` also needs no `close_connection` arm.)

## Limitations

0. **No UDP tracker, no IPv6 compact peers, no `min interval`, no `tracker id`.** Compact
   IPv4 peers (BEP 23) are supported; BEP 7's `peers6` is not, and IPv6 peers passed to
   `send_announce_response` with `compact` set are silently dropped.

1. **Stateless by design**: Each connection is independent. LLM must track peers across connections using conversation
   history or scheduled tasks.

2. **No persistent storage**: Peer lists exist only in LLM context. Server restart = empty tracker (unless LLM uses
   filesystem or external storage).

3. **No UDP support**: Only HTTP tracker protocol. UDP tracker (BEP 15) not implemented.

4. **No IPv6 compact format**: Compact peer format only supports IPv4 (6 bytes per peer). IPv6 requires 18 bytes per
   peer (BEP 7 `peers6`).

5. **Single-threaded LLM calls**: One LLM call per request. High-traffic trackers may experience latency.

6. **No authentication**: No built-in support for private tracker authentication (can be added via LLM logic).

## Security Considerations

- **Input validation**: nothing validates that the client's `info_hash` is 20 bytes; it is
  percent-decoded and hex-encoded as-is. A malformed request line gets HTTP 400 (it used
  to close the connection with no reply at all)
- **Port range**: Clients can announce any port (no validation)
- **IP spoofing**: No verification of client IP (uses peer-provided IP or connection IP)
- **DoS protection**: No rate limiting (LLM could implement via scheduled tasks)

## Testing

See `tests/server/torrent_tracker/CLAUDE.md` for comprehensive testing documentation.

## References

- [BEP 3: The BitTorrent Protocol Specification](http://www.bittorrent.org/beps/bep_0003.html)
- [BitTorrent Tracker Protocol](https://wiki.theory.org/BitTorrentSpecification#Tracker_HTTP.2FHTTPS_Protocol)
- [Bencode Encoding](https://wiki.theory.org/BitTorrentSpecification#Bencoding)
