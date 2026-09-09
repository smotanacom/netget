# RTP Protocol Implementation

RTP (Real-time Transport Protocol, RFC 3550) media server. UDP. **Experimental.**

## The idea (VNC for audio)

The model never emits samples or bytes. It answers a received-packet event with a **structured
description** of what a stream should carry — a tone at a frequency, DTMF digits, silence — and
`media.rs` synthesizes G.711 and frames it into correct RTP. This mirrors VNC, where the model
describes a screen and Rust owns the pixels.

## Files

- `media.rs` — the synthesis + packetization engine (shared with `rtsp`, and with `sip` when both
  features are on). G.711 µ-law/A-law encoders (ITU-T G.711 reference), tone/DTMF/silence
  synthesis, `RtpPacketizer` (running seq/ts, marker on first frame), RTP parse, RTCP SR builder.
- `mod.rs` — UDP accept loop. Parses inbound datagrams (RTP vs RTCP via the 200–204 packet-type
  reservation, RFC 5761 §4), emits an event, and on the model's `send_rtp_audio` action synthesizes
  and sends paced 20 ms RTP frames back to the peer.
- `actions.rs` — `ProtocolActions`: metadata, two events, two actions.

Wire I/O lives in `mod.rs` reading the raw action JSON (the SIP pattern); `execute_action` only
validates so a malformed action is reported, not sent.

## Events (both emitted)

- `rtp_packet_received` → actions `send_rtp_audio`, `send_rtcp_sender_report`
- `rtcp_packet_received` → action `send_rtcp_sender_report`

## What actually works

- **PCMU (PT 0) and PCMA (PT 8)** genuinely synthesize; ffmpeg decodes the output as `pcm_mulaw`/
  `pcm_alaw` at 8000 Hz. Validated by decoding the raw output with ffmpeg and by ffprobe pulling a
  stream through the RTSP front door.
- RTP header fields are correct: V=2, PT, incrementing sequence, timestamp advancing by frame size
  (160), stable SSRC, marker bit on the first frame of a burst.
- RTCP is a **minimal Sender Report** (no reception report blocks).

## What does NOT work / out of scope

- **No video codec.** Video payload types are not implemented.
- **No speech synthesis.** There is no TTS. Ask for a tone/DTMF/silence, or supply raw codec bytes
  hex-encoded (`content:"raw"`, `encoding:"hex"`, `samples`) — the only base-N path, decoded for real.
- No jitter buffer, no SRTP, no RTP retransmission, no receiver-report statistics.

## Encoding rule

Media is the one place binary is unavoidable. Prefer the structured description. When raw bytes are
truly needed, they are **hex-encoded and decoded for real** (`hex::decode`), never sniffed and never
base64 — the `send_tcp_data` lesson.

## The model is behind a budget, because RTP is high-rate

A single G.711 stream is **50 packets per second**. One `call_llm` per inbound datagram — which
is what this used to do — exhausts the model budget in seconds; and because one consultation can
authorize up to 30 s of outbound media from a 12-byte request, a spoofed source address makes the
server an amplifier as well. Two gates now stand in front of `call_llm`, in the shape
`src/server/tuntap/` established:

```text
  datagram ─▶ GATE 1  a deterministic handler answers  ── yes ──▶ no model call, never charged
             │        (script / static / manual rule)
             └─▶ GATE 2  a rolling per-minute budget   ── over ──▶ dropped, nothing sent,
                         (`llm_max_per_minute`, 30)                decision=fail_closed_rate_limited
```

- **`llm_max_per_minute`** is the one startup parameter (`RtpConfig::from_params`), default 30.
  `0` forbids model consultation outright, which is the right setting for a server driven wholly
  by handlers.
- Gate 1 is what makes a low ceiling usable rather than crippling: script and static handlers are
  the intended way to run RTP at rate, and they are never charged.
- It is a **sliding window**, not a leaky bucket, so "never more than N in any minute" is
  literally true rather than an average that permits bursts.
- Over-budget datagrams get the same silence as every other RTP failure. They are counted, and
  the refusals are reported once per burst rather than once per packet — the status channel is
  unbounded, so a line per dropped packet would be its own denial of service.

`tests/server/rtp/budget_test.rs` measures all of it, including the control that makes the zeros
mean something: at `llm_max_per_minute: 0` a static handler still streams.

## One connection entry per peer, not per datagram

The accept loop used to `add_connection_to_server` and push `__UPDATE_UI__` for **every**
datagram, so a 50 pps stream added 50 rows and 50 messages a second to the unbounded status
channel — and `send_rtp_audio`'s remote-address lookup then picked the first of a hundred
identical entries. `RtpServer::track_peer` bumps the existing entry for a peer and creates one
only for a peer it has not seen. The `.connectionless()` idle sweep reclaims them.

## Duration caps apply however the content was described

`synthesize` refuses anything past `MAX_DURATION_MS` (30 s). DTMF takes its length from the digit
count (200 ms each) rather than from `duration_ms`, so the cap did not constrain it and a long
`digits` string from a script or static handler allocated without bound; it is now held to the
same 30 s ceiling (150 digits).

## Fail-closed

On LLM failure the server sends nothing (RTP has no error frame) and logs on both channels. It never
falls through to a default stream — media the model never authorized must not appear on the wire.

Silence is deliberate here, and it is the one place in the tree where it is the *correct* answer to a
backend failure. RTP is a one-way media transport: there is no request/response turn, no error frame,
and the peer is not blocked waiting on us. The only in-band thing we could send is an RTCP BYE, which
would assert we are leaving a session we never joined — and at a 20 ms frame interval it would be one
bogus packet per inbound datagram. So nothing goes on the socket.

What the silence must not do is hide *which* silence it was, since all three look identical on the
wire. `handle_datagram` tags each in the log, radius-style:

- `decision=model_sent_nothing` — the model answered and asked for no media (a real answer).
- `decision=fail_closed_backend_overloaded` — the backend was saturated (`WireFailure::Overloaded`).
- `decision=fail_closed_backend_error` — anything else (`WireFailure::Unavailable`).

There is no `decision=model_reject`: RTP has no accept/deny semantics, so a model declining to stream
*is* `model_sent_nothing`. The error itself goes only to the log and the status stream — never into a
packet.
