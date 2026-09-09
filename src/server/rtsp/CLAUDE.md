# RTSP Protocol Implementation

RTSP (Real-Time Streaming Protocol, RFC 2326) control server over TCP. **Experimental.**
Feature `rtsp` implies `rtp` (`rtsp = ["rtp"]`) because PLAY reuses the RTP media engine.

## Role

The control front door to NetGet's RTP media. A real client (ffprobe, ffplay, VLC) runs
OPTIONS → DESCRIBE → SETUP → PLAY → TEARDOWN. **NetGet owns the RTSP framing** (CSeq, Transport,
Session, RTP-Info, and the RTP UDP port allocation) deterministically; the **model shapes the
DESCRIBE SDP, gates status codes, and decides what PLAY streams** (tone/DTMF/silence).

## Flow

1. `SETUP` — parses the client's `Transport: ...;client_port=A-B`, binds a server-side RTP UDP
   socket, records the client RTP address, returns a well-formed `Transport` (with `server_port`)
   and a `Session` id. Session state is per-TCP-connection local (RTSP is one sequential connection).
2. `PLAY` — synthesizes G.711 via `crate::server::rtp::media` and streams paced 20 ms RTP frames to
   the client's RTP port. This is what makes an RTSP session carry real media.
3. `TEARDOWN` / connection close — aborts the streaming task.

Wire responses are built in `mod.rs` from the raw action JSON; `execute_action` only validates.

## Events (all emitted)

`rtsp_options`, `rtsp_describe`, `rtsp_setup`, `rtsp_play`, `rtsp_teardown`, `rtsp_other` — each with
its own response action attached.

## Validated

ffprobe (`-rtsp_transport udp`) completes OPTIONS→DESCRIBE→SETUP→PLAY and reports
`Audio: pcm_mulaw, 8000 Hz, mono`. Mocked E2E asserts status lines, the DESCRIBE SDP, and that RTP
actually arrives on the negotiated UDP port.

## Backend failure (fail-closed contract)

When `call_llm` returns `Err`, the request is answered rather than dropped, and the answer
carries a **category only** — `crate::utils::WireFailure` classifies the error and nothing
derived from it reaches the socket (no backend URL, model name, path or `anyhow` chain; those
go to the log and the status stream). `Overloaded` → `503 Service Unavailable` with
`Retry-After: 5` so a client backs off; anything else → `500 Internal Server Error`.

Three outcomes are separated in the log by a `decision=` tag, the `radius` shape:

- `decision=model_reject` — the model set a non-2xx `status_code`.
- `decision=model_no_answer` — the model was reachable and returned no action.
- `decision=fail_closed_llm_overloaded` / `decision=fail_closed_llm_error` — the call errored.

`reason_phrase` returns a category phrase for any code it does not name (4xx → `Client Error`,
5xx → `Server Error`), so a refusal cannot go out as the self-contradicting `RTSP/1.0 403 OK`.

**Known fail-open, not addressed here:** on `decision=model_no_answer` the RTSP defaults still
apply — OPTIONS advertises the built-in method list, DESCRIBE serves `default_sdp()`, SETUP mints
a session and PLAY streams a 440 Hz tone. Silence from the model is therefore indistinguishable
on the wire from approval. That is a separate fail-open defect from the backend-failure path
above; it is logged but not yet refused.

## Dashboard injection (`[ message this peer ]` / `[ disconnect this peer ]`)

Every accepted connection registers a peer handle (`server::peer_support`) as soon as its task
starts, so the operator can reach it immediately. The reader and the peer-injection task share one
`Arc<Mutex<WriteHalf>>`, and `update_connection_stats` fires on every TCP read and write (plus the
streamed RTP is counted against the same connection), so the rail's `↓ ↑` counters and
`last_activity` are live. The handle is removed on every exit path (EOF, read error, 1 MiB
overflow, injected close) through the single cleanup in `handle_connection`.

**The useful injection is `close_connection`** — `execute_action` has an explicit arm returning
`ActionResult::CloseConnection`, which the generic peer task half-closes (the reader then sees
EOF). An injected `rtsp_*_response` verb writes **nothing**: RTSP framing (CSeq, Transport,
Session, RTP-Info) is built in `mod.rs` from the request, so `execute_action` returns `NoAction`
for the response verbs and there is no request context to frame an unsolicited response against.
This is not a `Custom`-result gap; it is inherent to the response verbs being request-bound.
The redis-style refactor (move the encoder into `execute_action`, return `Output`) does not
apply: `build_response` needs the request's CSeq, and SETUP/PLAY responses additionally need
the per-connection `Session` state (session id, negotiated RTP ports), which lives in
`run_connection` — an unsolicited response with no CSeq would be discarded or misattributed
by the client. What *would* be genuinely injectable is new server-initiated vocabulary (RFC
2326 defines server→client requests such as ANNOUNCE and REDIRECT, which this implementation
does not speak); adding one of those as an `Output`-returning verb is new protocol surface,
not an encoder move, and has not been done.
Test: `tests/server/rtsp/peer_inject_test.rs` (zero LLM calls).

## Out of scope

- **No TCP-interleaved transport** (RTP over the RTSP TCP channel). UDP RTP only, via
  client_port/server_port.
- Audio only (PCMU/PCMA); no video.
- No RTSP digest auth, no RECORD/ANNOUNCE, no PAUSE resume semantics (PAUSE lands in `rtsp_other`).

## Port

Default 8554 (unprivileged), privilege `None`. RFC's 554 is privileged; pass `port: 554` explicitly
if you hold the privilege — this implementation does not hardcode or require it.

## Three defects this file used to describe as working

- **The RTP socket was bound to `127.0.0.1`.** SETUP succeeded, the Transport header named a real
  `server_port`, PLAY answered 200 — and then every `send_to` failed for any client not on
  loopback, because a loopback-bound socket cannot reach an off-host address. The session looked
  established and no media ever arrived, which is the worst shape a failure can take. It now
  binds the unspecified address of the family the client reached us on.
- **`parse_rtsp_request` decoded the whole read buffer as UTF-8 before looking for the header
  terminator.** Anything the buffer merely happened to contain past the end of a valid request
  invalidated the request too — a multi-byte character split across two TCP segments, a binary
  body, a second pipelined request still arriving. The connection then sat with a fully-formed
  request unanswered until the 1 MiB overflow closed it: a head-of-line stall that reads as a
  hung server. The terminator is now found on the bytes and only the header block is decoded.
  `tests/server/rtsp/parser_test.rs` holds it.
- **A malformed media description on PLAY was silently replaced with a 440 Hz PCMU tone.** Both
  `AudioCodec::parse` and `media::parse_audio_content` had their errors discarded with `.ok()`,
  so a model asking for `content:"dtmf"` and forgetting `digits`, or naming a codec this engine
  cannot synthesize, got a confident `200 OK` and a tone it never asked for — with the reason
  thrown away. A value that fails to parse is now `400 Bad Request` plus
  `decision=fail_closed_bad_action`; an *absent* field still takes its documented default.

## What the model gates and what it does not

RTSP framing — CSeq, Transport, Session, RTP-Info, the status line — is owned by `mod.rs` and is
deterministic. The model shapes the DESCRIBE SDP, chooses status codes, and decides what PLAY
streams. An absent action still takes the method default (200, and a `default_sdp`), which is
right for a control protocol whose whole job is to hand out a well-formed session; it is *not*
the SIP situation, where a default would be an admission decision. A model refusal is an explicit
non-2xx and is logged `decision=model_reject`, distinct from `decision=model_no_answer` and from
the `fail_closed_llm_*` pair.

## Transport is UDP unicast only

`handle_setup` always answers `RTP/AVP;unicast;client_port=..-..`, whatever the client's
Transport header asked for. Interleaved (`RTP/AVP/TCP`) and multicast are not implemented, so a
client that requests one gets a UDP session it did not ask for rather than `461 Unsupported
Transport`.
