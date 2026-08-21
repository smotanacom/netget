# BitTorrent Tracker Client Implementation

## Overview

The BitTorrent Tracker client provides LLM-controlled HTTP-based communication with BitTorrent trackers for peer
discovery. Trackers coordinate peer connections by maintaining lists of clients downloading/seeding specific torrents.

## Protocol Details

**Protocol:** BitTorrent Tracker Protocol (BEP 3)
**Transport:** HTTP GET requests with bencode-encoded responses
**Port:** Typically 6969 or 80/443 for HTTP/HTTPS trackers
**Stack:** ETH > IP > TCP > HTTP > BitTorrent-Tracker

## Implementation

### Library Choices

- **HTTP Client:** `reqwest` - Standard async HTTP client (already used by HTTP client)
- **Bencode:** `serde_bencode` - Serialization/deserialization of bencoded data

### Architecture

1. **Stateless Connection:** Tracker client doesn't maintain persistent connections
2. **Request-Response:** Each announce/scrape is an independent HTTP GET request
3. **LLM Integration:** LLM receives responses and decides on follow-up actions

### Connection Flow

```
1. Client initialized with tracker URL
2. LLM triggers announce/scrape action
3. HTTP GET request sent to tracker
4. Bencode response parsed
5. LLM receives peer list/statistics
6. LLM decides: continue announcing, scrape, or disconnect
```

### Message Format

**Announce Request:**

```
GET /announce?info_hash=<hash>&peer_id=<id>&port=<port>&uploaded=<bytes>&downloaded=<bytes>&left=<bytes>&event=<started|completed|stopped>
```

**Announce Response (bencode):**

```
d8:intervali1800e5:peers<binary peer list or list of dicts>e
```

**Scrape Request:**

```
GET /scrape?info_hash=<hash>
```

**Scrape Response (bencode):**

```
d5:filesd20:<info_hash>d8:completei10e10:incompletei5eeee
```

## LLM Control Points

### Actions

1. **tracker_announce** - Announce presence to tracker
    - Parameters: info_hash, peer_id, port, uploaded, downloaded, left, event
    - LLM decides: when to announce, what event type, what statistics to report

2. **tracker_scrape** - Query tracker statistics
    - Parameters: info_hash
    - LLM decides: which torrents to scrape

3. **disconnect** - Stop tracking
    - LLM decides: when to stop announcing

### Events

1. **tracker_announce_response** - Received peer list from tracker
    - Data: interval, complete (seeders), incomplete (leechers), peers
    - LLM analyzes: peer list, decides to connect to peers or re-announce

2. **tracker_scrape_response** - Received torrent statistics
    - Data: file statistics (complete, incomplete, downloaded)
    - LLM analyzes: popularity, health of torrent

## Limitations

1. **No UDP tracker support** - Only HTTP/HTTPS trackers (UDP trackers use different protocol)
2. **Simplified peer parsing** - Binary compact peer format not fully parsed
3. **No tracker tier support** - Single tracker only (multi-tracker not implemented)
4. **No automatic re-announce** - LLM must explicitly trigger re-announces

## Testing Strategy

See `tests/client/torrent_tracker/CLAUDE.md` for E2E testing details.

## Example LLM Prompts

```
"Connect to tracker at http://tracker.example.com:6969/announce and announce with info_hash abc123, peer_id -TR2940-xyz, port 6881, event started"

"Scrape statistics for info_hash abc123 from tracker at http://tracker.example.com:6969/scrape"

"Re-announce with completed event to indicate finished download"
```

## References

- [BEP 3: The BitTorrent Protocol Specification](http://www.bittorrent.org/beps/bep_0003.html)
- [BEP 23: Tracker Returns Compact Peer Lists](http://www.bittorrent.org/beps/bep_0023.html)
- [Tracker Protocol Specification](https://wiki.theory.org/BitTorrentSpecification#Tracker_HTTP.2FHTTPS_Protocol)

## Injected actions (the dashboard's `[ send ]`)

A tracker client has no read loop at all - each announce/scrape is a one-shot HTTP GET - so
the command channel is the *only* way to reach a running one. It is registered and its
`TorrentTrackerClient::command_loop` spawned **before** the connect-event LLM call, which a
`*` -> manual rule can park for minutes.

The old `execute_tracker_action` is now `TorrentTrackerClient::apply_action`, taking an
already executed `ClientActionResult`; the connect-event task and the command loop both call
it, so an injected `tracker_announce` builds the identical announce URL and fires the same
`tracker_announce_response` event.

`ClientSendOutcome` semantics:

| Outcome | When |
|---|---|
| `Executed { detail }` | The announce/scrape completed (`tracker_announce completed`), or it was sent and the tracker's reply could not be bdecoded - `detail` says which. |
| `Rejected { error }` | The action is not `tracker_announce`, `tracker_scrape` or `disconnect`. |
| `Disconnected` | `{"type":"disconnect"}`; status goes to Disconnected and the handle is dropped. |
| `Err(...)` | The HTTP GET failed, or a required field (`info_hash`, `peer_id`, `port`) was missing. |

**`Sent { bytes_sent }` is never reported**: reqwest owns the socket. The GET is awaited
before the outcome is returned.
