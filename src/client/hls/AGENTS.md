# HLS client

`remote_addr` is the stream's playlist URL (`http://host:port/path.m3u8`); a bare `host:port`
means `/index.m3u8`. Every URI a playlist names is resolved against that playlist (RFC 3986)
and refused if it leaves the stream's origin, so a playlist cannot point the client at another
host. Requests go through the shared `http_fetch` client (reqwest natively, same-origin
redirects only).

## Actions and events

- `hls_get_playlist {uri?}` → `hls_playlist`. A master playlist reports its variants
  (bandwidth, average bandwidth, resolution, codecs, frame rate) and `EXT-X-MEDIA`
  renditions; a media playlist reports target duration, media sequence, playlist type,
  `endlist`, the first `MAX_LISTED_SEGMENTS` (50) segments, the count, the summed EXTINF
  duration, a non-`NONE` `EXT-X-KEY` method and an `EXT-X-MAP` URI. A media playlist read
  becomes the base later segment URIs resolve against.
- `hls_get_segment {uri}` → `hls_segment`: size, container (`mpegts`, `fmp4`, `packed` for an
  ID3-led audio segment, `unknown`), and for MPEG-TS the packet count, sync errors, the PMT's
  elementary streams (PID, stream type, codec name), continuity-counter gaps (the
  discontinuity indicator is honoured) and the media length by PES timestamps (the widest PTS
  span of any stream).
- `hls_play {uri?, variant?, max_segments?}` → `hls_played`. A master playlist's variant is
  chosen (`highest`/`lowest` bandwidth or an index; default highest), then segments are
  fetched in sequence order up to `max_segments` (default 10, at most 100). A playlist without
  `EXT-X-ENDLIST` is live: it is reloaded after the target duration (half of it when nothing
  new appeared, RFC 8216 §6.3.4) and segments already fetched are skipped by sequence number.
  Sequence numbers that slid out of the window before they were fetched are counted as
  `gaps`; a live playlist that does not grow for `MAX_IDLE_RELOADS` (6) reloads ends the run
  with an error. The summary: segments, first and last sequence, gaps, reloads, `endlist`,
  bytes, timestamp duration, EXTINF duration, containers, codecs, continuity errors and the
  segments that failed.

Injected actions (`send_to_client`) answer `Executed` with the event's data.

## Bounds

Playlist 1 MiB and 20 000 lines, segment 32 MiB (`max_inbound_bytes`), 100 segments per run,
8 follow-ups in a handler chain.

## Not implemented

Decryption (an encrypted stream's segments are fetched and counted, not analysed), fMP4
sample analysis, LL-HLS partial segments and blocking reloads, alternative-rendition playback,
byte ranges (`EXT-X-BYTERANGE` segments are fetched whole).
