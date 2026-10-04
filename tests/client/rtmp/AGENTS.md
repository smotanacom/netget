# RTMP client tests

`peer_test.rs` runs MediaMTX 1.21.1 (independent Go RTMP, unchanged) with FFmpeg publishing the
looped clip into `live/cam`. NetGet's client plays it for three seconds (Play.Start, avc and aac,
message and keyframe counts, rising timestamps), and a second client publishes the clip as
`live/netget` while MediaMTX's API is polled until the path is ready with H.264 and MPEG-4
Audio tracks. Needs `NETGET_RTMP_MEDIAMTX` from `tests/server/rtmp/install_peers.py` and FFmpeg.
