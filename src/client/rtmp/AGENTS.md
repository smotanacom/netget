# RTMP client — Experimental

Uses `src/server/rtmp/chunk.rs` and `src/server/rtmp/amf0.rs`. Connect performs the handshake,
sets its chunk size to 4096 and sends connect for `app`; `_error` fails the connect. A reader
task reassembles chunks (reassembly is not cancel-safe); the session acknowledges at the
server's window and answers pings. Actions: `rtmp_play` (createStream, play, buffer length;
for `seconds`, tallying onStatus codes, onMetaData, codecs from the first audio and video
messages, message and keyframe counts, first and last timestamps, other data messages; then
deleteStream) and `rtmp_publish` (an operator-supplied FLV of at most 64 MiB, its script data
sent as `@setDataFrame onMetaData`, tags paced by timestamp unless `realtime` is false, then
FCUnpublish and deleteStream). Each ends with one `rtmp_report`. Every command waits at most
15 s overall for its answer, however much media keeps arriving. Some servers (MediaMTX) treat
a connection as one reader or one publisher, so play and publish belong on separate clients
there.
