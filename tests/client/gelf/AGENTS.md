# GELF emitter tests

Both transport command loops are tested for typed sends, atomic rejection, disconnect/stop,
manual connected-handler injection, IPv6 UDP, TCP remote EOF and script/common-memory actions.
The server suite pairs both NetGet roles across UDP none/gzip/zlib and TCP, including chunks.

Independent receiver test uses Graylog2/go-gelf v2 branch master pinned at
25db8704bcf3f484c958312cd0cc49e5c768dcf1. `install_peers.py` lists exact sha256 per original
Go file. Upstream `Reader.ReadMessage` performs UDP chunking and compression detection plus
JSON decoding. Upstream private `TCPReader` performs NUL framing and `Message.UnmarshalJSON`;
`peer_export.go.txt` only exports start/address/read methods in the same package. Main prints
observed typed fields, with no replacement framing/parser. Two consecutive messages include
Unicode, multiline full_message, fractional timestamps, severity and string/numeric extras;
UDP none forces multiple chunks, and gzip/zlib are independently decoded. Tests fail when
NETGET_GELF_READER or pygelf is absent; bootstrap in the server test notes.

The upstream reference UDP reader serializes one message at a time and does not support our
cross-source/reorder/conflict/lifetime policy; those are negative/bounds source-wire tests.
Its TCP helper has an EOF busy-loop defect, so the peer process exits after its fixed message
count and a 30s deadline, with kill-on-drop cleanup. No complete Graylog platform, fuzz,
Wireshark, TLS, HTTP or delivery/persistence evidence is claimed.
