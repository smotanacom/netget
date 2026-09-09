# RTP E2E Tests

`e2e_test.rs` — `test_rtp_synthesizes_pcmu_reply`.

## Strategy

Black-box over a real `tokio::net::UdpSocket` (async, because `#[tokio::test]` is current-thread and
a blocking recv would park the mock LLM). Send a crafted 12-byte RTP packet; the mock answers
`rtp_packet_received` with `send_rtp_audio` (440 Hz PCMU, 60 ms), **echoing the caller's SSRC via
`.respond_with_actions_from_event`** — the required dynamic-echo pattern for UDP-style protocols so a
static id never mismatches.

## Assertions (protocol-level, not "bytes arrived")

Parses the returned RTP header: version 2, PT 0 (PCMU), 160-sample payload (20 ms frame), SSRC
echoed. Reads a second packet and asserts the sequence increments by 1 and the timestamp by 160.

## LLM call budget

1 startup + 1 event = **2 calls.** Ends with `verify_mocks().await?`.

## Real client

The G.711 output is independently validated by decoding with ffmpeg (`pcm_mulaw`), and end-to-end by
ffprobe through the RTSP suite. Localhost only.

---

## budget_test.rs — the model is not on the per-packet path

A G.711 stream is 50 packets per second and one consultation can authorize 30 s of outbound
media, so `llm_max_per_minute` is what stops a stream from being 50 LLM calls a second and a
spoofed source address from being an amplifier. Four tests, **zero LLM calls** (the endpoint is
a closed port, so any consultation that does happen fails fast rather than hanging):

- `rtp_over_budget_sends_nothing` — at `llm_max_per_minute: 0`, three packets get silence. RTP
  has no error frame, so refusing must not fall through to a default stream.
- `rtp_static_handler_is_never_charged_against_the_budget` — **the control.** At the same ceiling
  of zero, a static handler streams a real PCMU packet. Without it the test above is
  indistinguishable from a server that never streams anything.
- `rtp_budget_admits_exactly_the_ceiling_then_refuses` — the window itself, against a supplied
  clock so it needs no sleeping: 3 admitted, the 4th refused, still refused at 59 s, exactly one
  slot reopens at 60 s. That is what makes it a sliding window rather than a bucket that refills
  continuously and permits bursts above the ceiling.
- `rtp_budget_of_zero_admits_nothing` — zero means zero, including the first call.
