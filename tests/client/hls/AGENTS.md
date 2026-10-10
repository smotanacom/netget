# HLS client tests

Two independent HLS writers, each served by Python's `http.server`, whose request log is read
back to see what NetGet fetched. ffmpeg and gst-launch-1.0 (GStreamer with the good, bad and
ugly plugin sets) are required; the tests fail rather than skip without them.

- `netget_plays_an_ffmpeg_vod_stream_with_variants`: ffmpeg's HLS muxer writes a 4 s VOD
  stream with a master playlist and two variants (320x240 and 160x120, H.264 + AAC, one-second
  segments). A chain reads the master playlist and plays the highest variant. Asserted: both
  variants with resolution, bandwidth order and codecs; the run chose variant 0, fetched
  segments 0–3 with no gap or reload, saw H.264 and AAC in MPEG-TS with no continuity errors,
  3.5–4.1 s by timestamps, and exactly the bytes ffmpeg wrote; the server log shows the
  variant's four segments fetched and nothing of the other. An injected `hls_get_segment`
  resolves against the media playlist and reports the file's size, packet count and PIDs; a
  URI on another origin is refused.
- `netget_follows_a_live_gstreamer_playlist_as_it_slides`: GStreamer's `hlssink2` writes a
  live stream (H.264 + MP3, one-second segments, a three-segment playlist window). A chain
  plays five segments: at least one reload, five consecutive sequence numbers with no gap, no
  `ENDLIST`, 4–5.6 s by timestamps, and the server log shows those five segments fetched.

Mutation-checked: dropping the model's actions fails both. No LLM calls.
