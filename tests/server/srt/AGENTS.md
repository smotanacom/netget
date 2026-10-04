# SRT tests

Peer: libsrt's srt-live-transmit (1.4 or newer) and FFmpeg from the system;
`python3 tests/server/srt/install_peers.py ROOT` checks them and prints
`NETGET_SRT_LIVE_TRANSMIT`. MediaMTX is not used: its gosrt refuses srt-tokio's SRT 1.3
handshake. `tests/helpers/srt.rs` holds the policy, the FFmpeg clip, process-group pipelines and
a synthetic MPEG-TS stream.

- `peer_test.rs` — FFmpeg paces the clip into a libsrt caller publishing `live/cam`; a second
  libsrt caller reads four seconds and ffprobe finds H.264 and AAC; a forbidden resource is
  refused (libsrt prints the rejection); the publisher's closing statistics.
- `wire_test.rs` — the stream-ID parser; empty, forbidden and bidirectional stream IDs and a
  second publisher refused in the handshake; NetGet's publisher relayed to NetGet's reader
  byte-for-byte (602 TS packets, the PMT's stream types), an injected text, the idle close with
  its statistics (86 messages); no handler answer.

`tests/client/srt/peer_test.rs` — NetGet's caller against libsrt listeners.
