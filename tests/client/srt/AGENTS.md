# SRT client tests

`peer_test.rs` runs libsrt's srt-live-transmit as a listener twice: once sending a paced
H.264/AAC MPEG-TS stream (1316-byte chunks), which NetGet's caller reads for three seconds and
reports with its PMT stream types, and once writing what NetGet's caller publishes from the clip,
which ffprobe then decodes. Needs `NETGET_SRT_LIVE_TRANSMIT` from
`tests/server/srt/install_peers.py` and FFmpeg.
