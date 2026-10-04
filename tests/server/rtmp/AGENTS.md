# RTMP tests

Peers: `python3 tests/server/rtmp/install_peers.py ROOT` installs MediaMTX 1.21.1 (the release
binary for this platform, hash-pinned against the release's checksums) and prints
`NETGET_RTMP_MEDIAMTX`; FFmpeg (ffmpeg, ffprobe) comes from the system and its absence fails the
tests. `tests/helpers/rtmp.rs` holds the policy, the FFmpeg-made H.264/AAC clip, MediaMTX's
configuration and a synthetic FLV.

- `peer_test.rs` — FFmpeg publishes the clip (looped); ffprobe reads H.264 320x240 and AAC;
  FFmpeg decodes two seconds; MediaMTX pulls the stream (empty stream name, path as app) and
  reports H.264 and MPEG-4 Audio tracks; the publish summary; a refused stream key, app and play.
- `wire_test.rs` — the handshake echo, a refused connect, the connect result, a ping, an AMF3
  command and an oversized message closing the connection, a connect with no handler answer;
  NetGet's publisher and player with cached metadata, an injected onTextData and the summary.

`tests/client/rtmp/peer_test.rs` — NetGet's client against MediaMTX.
