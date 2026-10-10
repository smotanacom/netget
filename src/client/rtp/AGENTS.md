# RTP endpoint (client)

Sends to `remote_addr`, receives on `listen` (default `127.0.0.1:0`; `connect` returns it).
Synthesis and framing are the RTP server's (`src/server/rtp/media.rs`): the model describes
audio (`send_rtp_audio {payload_type pcmu|pcma, content tone|dtmf|silence|raw, tone_hz,
digits, duration_ms}`, the server's own action definition) and Rust synthesizes G.711 and sends
it as 20 ms packets paced in real time from a registered task. One stream at a time: a new
`send_rtp_audio` (or `disconnect`) cuts the current one short, and `rtp_sent {ssrc, packets,
duration_ms, complete}` reports each. `send_rtcp_sender_report` sends a minimal SR.

## Inbound

RTP arrives at 50 packets a second, so it is **summarised, never reported per packet**: per
SSRC, `rtp_stream_started {ssrc, payload_type, codec, from}` on the first packet and
`rtp_stream_ended {ssrc, packets, lost, octets, duration_ms, tone_hz, level_dbfs}` after
`STREAM_IDLE` (1 s) of nothing. Loss comes from sequence numbers (wrap-aware), length from
timestamps. G.711 payloads are decoded (ITU-T µ-law and A-law) and up to
`MAX_ANALYSED_SAMPLES` analysed: the dominant frequency from zero crossings with hysteresis,
and the RMS level. RTCP arrivals are `rtcp_received {packet_type, ssrc, from}`. At most
`MAX_STREAMS` streams are tracked.

No SRTP, no jitter buffer, no receiver reports, audio only. A chain stops after
`MAX_FOLLOWUP_DEPTH` (8).
