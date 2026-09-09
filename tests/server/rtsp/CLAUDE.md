# RTSP E2E Tests

Built with `rtsp` + `rtp`.

## `e2e_test.rs` — `test_rtsp_setup_play_streams_rtp`

Drives OPTIONS → DESCRIBE → SETUP → PLAY over a real `TcpStream`, with a client-side UDP socket whose
port is advertised in SETUP's `Transport: client_port=`. Asserts:

- OPTIONS → 200 with `Public:`
- DESCRIBE → 200, `application/sdp`, an `m=audio` line, `PCMU/8000`
- SETUP → 200, `Transport:` echoing our `client_port`, a `server_port`, a `Session` id
- PLAY → 200, `RTP-Info`
- **Real RTP arrives** on the negotiated UDP port: version 2, PT 0, 160-sample frame.

Mock: 1 startup + 4 method events = **5 calls.** Ends with `verify_mocks().await?`.

## `ffprobe_test.rs` — `ffprobe_reads_rtsp_stream` (`#[ignore]`)

Real-client validation. Shells out to `ffprobe -rtsp_transport udp`, which performs a genuine
OPTIONS→DESCRIBE→SETUP→PLAY and reports `Audio: pcm_mulaw, 8000 Hz, mono`. `#[ignore]` because CI
runners lack ffmpeg; run manually:

```bash
./cargo-isolated.sh test --no-default-features --features rtsp,rtp \
    --test server -- --ignored --test-threads=1 rtsp::ffprobe
```

Localhost only.

---

## parser_test.rs — a complete request behind non-text bytes

`parse_rtsp_request` used to decode the **whole** read buffer as UTF-8 before looking for the
header terminator, so anything the buffer happened to contain past the end of a valid request
invalidated the request too — a multi-byte character split across two TCP segments, a binary
body, a second pipelined request still arriving. The connection then sat with a fully-formed
request unanswered until the 1 MiB overflow closed it: a head-of-line stall that reads as a hung
server.

Two tests, in-process with a static handler and **zero LLM calls**:

- `rtsp_answers_a_plain_request` — the control. A lone OPTIONS is answered 200 with its CSeq
  echoed. Without it, the next test proves nothing.
- `rtsp_answers_a_complete_request_followed_by_non_utf8_bytes` — the same OPTIONS with a trailing
  `0xFF` in the same segment. `0xFF` is never valid UTF-8 and is exactly what the leading byte of
  a split multi-byte character looks like to the old whole-buffer decode.
