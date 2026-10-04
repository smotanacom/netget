# RTMP server — Experimental

`chunk.rs` is the RTMP 1.0 transport: the simple version-3 handshake (S2 echoes C1; the digest
handshake is not implemented), chunk basic headers for every chunk stream ID range, message
headers of all four formats with extended timestamps, reassembly per chunk stream, Set Chunk
Size both ways (at most 1 MiB), Abort, and a writer. `amf0.rs` converts AMF0 to and from JSON
(depth 16, 10 000 values; AMF3 is refused).

Rust owns:
- The NetConnection flow: connect (one per connection; Window Acknowledgement Size, Set Peer
  Bandwidth, Set Chunk Size 4096, `_result` NetConnection.Connect.Success, objectEncoding 0),
  createStream (16 per connection), releaseStream / FCPublish / FCUnpublish, getStreamLength
  (0 for live), ping responses, acknowledgements at the peer's window, deleteStream /
  closeStream; anything else with a transaction ID gets `_error` NetConnection.Call.Failed.
- Stream naming: app/stream, or — for clients that send the whole path as app and an empty
  name, as MediaMTX does — the app itself, so both name the same stream.
- The live relay: one publisher per stream (a second is NetStream.Publish.BadName), up to 64
  players; onMetaData (from `@setDataFrame`) and the AVC/HEVC/enhanced and AAC sequence headers
  are cached and sent to each player first; a player's video starts at a keyframe; timestamps
  are rebased per player; a player whose 1024-message queue fills is dropped. Players may join
  before the publisher. Nothing is recorded.
- Bounds: 8 MiB messages (refused from the announced length), 64 chunk streams, a handshake
  deadline and `idle_timeout_secs` for connections that are not playing.

The handler answers `rtmp_connect`, `rtmp_publish` and `rtmp_play` with `rtmp_accept` or
`rtmp_reject` (NetConnection.Connect.Rejected, NetStream.Publish.Denied,
NetStream.Play.StreamNotFound with its description); no answer is a refusal. `rtmp_publish_ended`
reports what a publisher sent. A connection's peer handle accepts `rtmp_send_data` (an AMF0
data message — onTextData, onCuePoint — into every player of a publisher's stream, or to one
player) and `disconnect`.

Not implemented: RTMPS, RTMPT, the complex handshake, AMF3, recording or VOD, enhanced-RTMP
multitrack, authentication schemes inside connect.
