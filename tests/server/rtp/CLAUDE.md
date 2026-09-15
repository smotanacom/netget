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

---

## script_fallback_budget_test.rs — the answer decides the bound, and the calls are counted

The budget gate used to be decided by `find_handler`: a `Script` rule meant "exempt", and
`action_helper::call_llm` then dispatched that same handler itself, so a script that could not
answer (unknown language, missing interpreter, thrown exception) reached the model with the
budget already bypassed. At 50 pps that is 50 uncounted consultations a second with
`llm_max_per_minute` inert, each able to authorize 30 s of outbound media.

Unlike `budget_test.rs`, which points NetGet at a closed port, this file runs a **recording**
mock model and asserts on the call count. Five tests:

- `an_unclaimed_datagram_is_one_model_call` — **the control.** No rule, ample ceiling:
  5 datagrams, 5 calls. Without it every zero below is indistinguishable from a server nothing
  reached.
- `script_that_cannot_answer_reaches_the_model_when_the_budget_allows` — the same `*` script
  rule the old gate exempted, at a ceiling with room: 5 calls. These are exactly the calls the
  old code made at *every* ceiling.
- `script_that_cannot_answer_is_charged_to_the_budget` — identical configuration, ceiling 0:
  **0 calls.** Verified by reinstating the old gate, at which point it reports `got 5 call(s)`.
- `a_static_rule_still_answers_at_a_ceiling_of_zero` — a rule that *does* answer is still free.
  A gate that refused everything at 0 would pass the test above while destroying the
  configuration the ceiling exists to make usable.
- `the_configuration_and_the_answer_disagree` — the defect in one assertion:
  `RtpServer::configured_handler_kind` says `Some("script")` for the configuration whose handler
  answers `FallbackToLlm`. That helper is diagnostics-only and must never gate the budget again.

## LLM call budget

`script_fallback_budget_test.rs` deliberately **provokes** model calls and counts them: 10 across
the file (5 + 5), all to an in-process mock. The zero-call cases are the assertions.
