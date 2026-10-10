# RTSP client tests

`install_peers.py <dir>` installs mediamtx 1.9.3 (the release binary, SHA-256 checked, or the
same release built from the Go module proxy where GitHub is unreachable) and prints
`NETGET_MEDIAMTX`. ffmpeg comes from the system. The test fails without either.

`netget_plays_a_stream_from_mediamtx`: mediamtx on loopback ports of its own; ffmpeg publishes
a 1000 Hz PCMU tone to `/tone` over RTSP (ANNOUNCE/RECORD). A python chain makes NetGet
DESCRIBE (the SDP's audio track and `PCMU/8000` asserted), SETUP (a `server_port` in the
Transport) and PLAY. NetGet reports the stream starting; after two seconds an injected TEARDOWN
is answered 200, and the stream's summary has no loss, at least 1.5 s of media and a decoded
tone of 1000 Hz ± 15. OPTIONS lists DESCRIBE.

Mutation-checked: dropping the model's actions fails it. No LLM calls.
