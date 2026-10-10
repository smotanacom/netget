# RTSP client

RTSP/1.0 over one TCP control connection to `remote_addr` (`rtsp://host:port/path`, or
`host:port[/path]`; port 554 by default). Rust owns `CSeq`, the `Session` id (from the first
answer that carries one), `Content-Base`, and the `Transport`: each `rtsp_setup` binds an
even/odd UDP port pair on the control connection's local address (RFC 3550 §11) and offers it
as `RTP/AVP;unicast;client_port=P-P+1`. TCP interleaving is not implemented.

## Actions and events

`rtsp_options`, `rtsp_describe`, `rtsp_setup {track?}`, `rtsp_play {range?}`, `rtsp_pause`,
`rtsp_teardown`, `disconnect`. Every answer is `rtsp_response {method, status, reason, headers
(Public, Content-Base, Session, Transport, RTP-Info, Range), sdp?}`. DESCRIBE's SDP is parsed
into `{media: [{type, port, protocol, formats, rtpmap, control}], control}`, and its tracks'
control URLs are resolved against Content-Base (or the URL) so `rtsp_setup` without `track`
sets up the first audio or video track. PLAY, PAUSE and TEARDOWN go to the session URL.
TEARDOWN closes the RTP ports.

Media is summarised by the RTP client's `Tracker` (`src/client/rtp`), never per packet:
`rtsp_stream_started {ssrc, payload_type, codec, from}` and, after a second of nothing,
`rtsp_stream_ended {ssrc, packets, lost, octets, duration_ms, tone_hz, level_dbfs}` (G.711
decoded and analysed).

Responses are bounded (`MAX_HEADER_BYTES` 64 KiB, `MAX_BODY_BYTES` 1 MiB); at most
`MAX_PENDING` requests and `MAX_TRACKS` tracks; a chain stops after `MAX_FOLLOWUP_DEPTH` (8).
No authentication, no RTSP 2.0, no requests from the server.
