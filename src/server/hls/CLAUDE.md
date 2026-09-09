# HLS Protocol Implementation

HLS (HTTP Live Streaming, RFC 8216) server. **Experimental.**

Serves an `.m3u8` playlist and media segments over HTTP/1.1. The model decides the playlist
(structurally or as verbatim m3u8) and each segment's body.

## Why a self-contained HTTP reader (not the `http` server)

`mod.rs` carries a minimal HTTP/1.1 request reader rather than sharing the hyper-based `http`
server. HLS needs only method + path routing, and the `http` server's model is a single
`http_request` event — not the two distinct playlist/segment events HLS wants. The framing written
here is standard HTTP a real client (curl, ffplay) reads. One request per connection, `Connection:
close`.

## Routing

- path contains `.m3u8` → `hls_playlist_request` → `hls_playlist_response`
- anything else → `hls_segment_request` → `hls_segment_response`

Both events are always emitted, so both are reachable.

## Playlist rendering

`hls_playlist_response` accepts either a verbatim `playlist` string, or a structured `segments`
array (`[{uri,duration}]`) plus optional `target_duration`/`version`/`media_sequence`/`ended`, which
`render_playlist` assembles into a valid media playlist (`#EXTM3U`, `#EXT-X-TARGETDURATION`,
`#EXTINF`, `#EXT-X-ENDLIST`). Served as `application/vnd.apple.mpegurl`.

## Segment bodies and the binary rule

A segment is `hls_segment_response` with either `content` (UTF-8 text, for structural/placeholder
use) or `data` with `encoding:"hex"` for genuine binary — **the only sanctioned base-N path** for
real MPEG-TS bytes, and it is hex-decoded for real (`hex::decode`), never sniffed, never base64.
Default `Content-Type: video/mp2t`.

## What actually works / does not

- **The server does NOT synthesize MPEG-TS.** The m3u8 structure and the hex-decoded segment body
  are asserted by `test_hls_playlist_and_segment`, over a raw `TcpStream` with hand-written HTTP.
  The curl run (`tests/server/hls/curl_test.rs`) is `#[ignore]`d, so **no CI job establishes that a
  real client accepts this output** — treat the curl validation as a manual check, not a result.
- A real media player (ffplay/VLC) needs **valid segment bytes**, which the model must supply
  hex-encoded (e.g. a real `.ts`). Text segment bodies are for structural tests, not playback.
- No LL-HLS, no `#EXT-X-KEY` encryption, no multivariant master-playlist bitrate switching beyond
  what the model writes verbatim.

## Fail-closed

Every path that cannot produce media answers the peer — silence would leave a player blocked
until its own timeout — and the answer carries a **category only**, never an error string.
`HlsResponse::failure` builds it from `crate::utils::WireFailure`:

| Cause | Status | Body |
|---|---|---|
| LLM call errored, backend saturated (`WireFailure::Overloaded`) | 503 + `Retry-After: 5` | `netget: backend at capacity, retry later` |
| LLM call errored, anything else (`WireFailure::Unavailable`) | 500 | `netget: request could not be processed` |
| Model returned no action | 500 | same |
| Playlist action carried neither `playlist` nor `segments` | 500 | same |
| Segment action carried neither `data` nor `content` | 500 | same |
| Segment `data` was undecodable hex, or an unknown `encoding` | 500 | same |

The 503/500 split is the point: a player backs off on a transient overload and records a hard
fault otherwise. Collapsing them makes an outage look permanent.

Nothing derived from the error reaches the socket — not the backend URL, the model name, an
`anyhow` chain, the `hex` crate's decode message, or the model's own `encoding` string. All of
that goes to `tracing` and the status stream. `tests/wire_failure_test.rs` fails the build if the
leaked idioms reappear.

The empty-segment case is the one that used to be fail-*open*: an action with neither `data` nor
`content` was served as `200 video/mp2t` with `Content-Length: 0`, which a player accepts as a
valid (empty) segment — so the stream silently played nothing instead of reporting a fault.

### `decision=` tags in the log

Three failure causes must stay distinguishable to whoever reads `netget.log`, because an outage
is not a policy decision:

- `decision=model_reject` — the model itself chose a 4xx/5xx `status_code`
- `decision=fail_closed_no_answer` — the model answered nothing usable
- `decision=fail_closed_bad_action` — the model's action was malformed (bad hex, unknown encoding)
- `decision=fail_closed_llm_error` / `decision=fail_closed_llm_overloaded` — the LLM call failed

The success case is tagged `decision=model_answer`. Every tag also appears on the status stream
line for the response.

### Reason phrases

`reason_phrase` maps the common codes and falls back to a status-*class* phrase. The previous
blanket `_ => "OK"` framed a model-chosen 403 as `HTTP/1.1 403 OK`.

## Dashboard injection: connection stats yes, peer handle no

HLS is one-shot HTTP request/response — one read of the request, one write of the response, then
`handle_connection` returns and the socket closes (`Connection: close`). So it deliberately
registers **no** `peer_support` handle: the dashboard's `[ message this peer ]` /
`[ disconnect this peer ]` would have no live connection window to fire, and `execute_action`
needs no `close_connection` arm. The rail shows the dim "cannot message or disconnect a peer from
here yet" row for HLS connections, which is correct.

What it does do is refresh `AppState::update_connection_stats` on the single read and the single
write, so the rail shows real `↓ ↑` byte/packet counts and a fresh `last_activity` rather than
`↓0 ↑0`. Covered by the zero-LLM in-process test `hls_connection_stats_are_recorded`
(`tests/server/hls/e2e_test.rs`), which also asserts no peer handle is registered.

## It cannot read the filesystem, and that is the point

There is no `std::fs`, no `tokio::fs`, no `Path` and no `PathBuf` anywhere under
`src/server/hls/` — verify with `grep -rn "fs::\|File::\|PathBuf\|Path::" src/server/hls/`.
The request path is used for exactly two things: choosing between the playlist and segment
events (`path.contains(".m3u8")`), and being handed to the model as event data. Every byte the
server returns comes from the model's action — `playlist`/`segments` for a playlist, `content`
or hex-decoded `data` for a segment.

So there is **no path traversal surface**: `../../etc/passwd` is a string the model is asked
about, not a file that gets opened. A future change that resolves a path against a directory
would introduce the whole class at once, and would also break the "protocols must not implement
storage" rule — the model supplies the bytes.

## Request reading is bounded in both dimensions

Headers are capped at 64 KiB **and** at a 30-second deadline (`HEADER_READ_TIMEOUT`). The size
cap alone was not enough: a peer that connects and sends one byte, or nothing, parked the task
and its socket for as long as it cared to hold the connection open, which is the whole of
slowloris. The path is additionally truncated to `MAX_PATH_LEN` (512) before it reaches the log,
the status stream or the model's prompt, since all three used to take it at whatever length the
peer chose.

`parse_request_line` also finds the header terminator on the bytes rather than decoding the whole
buffer as UTF-8 first — the same head-of-line stall RTSP had, where a read that stopped in the
middle of a multi-byte character invalidated headers that were already complete.
