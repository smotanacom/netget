# NTP Protocol E2E Tests

## Test Overview

Three tests in `test.rs`, each starting its own NetGet NTP server against the mock Ollama harness and
then speaking real NTP to it. Two use `rsntp` as an off-the-shelf SNTP client; the third decodes all
48 bytes of the reply by hand.

## Test Strategy

**Strict validation.** Every reply is decoded and compared field by field to what the mocked handler
asked for. There is no fallback path and no "accepts any response" branch.

This is a deliberate reversal. The suite previously caught `rsntp`'s error, printed
"this may be expected if LLM doesn't fully implement NTP", and fell back to a raw socket whose three
outcomes — a response, an I/O error, and a timeout — were all printed and none asserted. The stratum
test parsed byte 1 and printed it without comparing it to anything. The suite therefore passed
whether the server answered correctly, answered garbage, or never answered at all. Do not reintroduce
a lenient branch: if a reply is wrong, that is the finding.

**`rsntp` must succeed.** Its checks are the interoperability-relevant ones (`src/core_logic.rs`):

- the reply's originate timestamp must equal the request's transmit timestamp *verbatim*
- mode must be 4 (server) or 5 (broadcast)
- stratum must not be 0 (that is Kiss-o'-Death)
- the transmit timestamp must be non-zero
- the reply's version must be 4, the version `rsntp` sends

A failure in any of those surfaces as a `SynchronizationError`, and the test fails with it.

**Use `AsyncSntpClient`, never the blocking `SntpClient`.** The mock Ollama server runs in-process on
the test's own tokio runtime, and `#[tokio::test]` is single-threaded by default. Blocking the
runtime inside `SntpClient::synchronize` deadlocks the very LLM call the reply depends on, so the
call always times out. That is almost certainly why the old fallback path existed.

## LLM Call Budget

- `test_ntp_basic_query`: 1 startup + 1 `ntp_request` = 2
- `test_ntp_time_sync`: 1 startup + 1 `ntp_request` = 2
- `test_ntp_stratum_levels`: 1 startup + 2 `ntp_request` = 3
- **Total: 7** (limit 10)

Every rule uses `expect_calls(1)`, so an extra or missing LLM round-trip fails `verify_mocks()`.

## Mock Expectations

Unlike DNS, DHCP and the other UDP protocols, NTP does **not** need
`respond_with_actions_from_event()`. The transaction-identifying value is the client's transmit
timestamp, and the server copies it into the reply itself: `spawn_with_llm_actions` reads bytes 40-47
and builds a per-request `NtpProtocol::for_request(origin, version)`. A static action list is
therefore correct, and the mock must **not** set `origin_timestamp` — leaving it unset is what proves
the server-side echo works.

`test_ntp_stratum_levels` runs two different handlers on one server by discriminating on
`and_event_data_contains("client_version", …)`: the v3 request gets a fully specified time response,
the v4 request gets `ignore_request`. Rules are matched in declaration order, first match wins.

## Client Library

**rsntp 4.1** (`AsyncSntpClient`), plus a hand-rolled 48-byte encoder/decoder in the test file.

`rsntp` exposes only `stratum()`, `leap_indicator()`, `reference_identifier()`, `datetime()`,
`clock_offset()` and `round_trip_delay()`. Poll, precision, root delay and root dispersion are not
reachable through it, which is why the third test decodes the packet directly.

One `rsntp` decoding quirk to know: the reference identifier is read as ASCII only when stratum is 0
or 1. At stratum 2 and above the same four bytes become an IPv4 address. `test_ntp_time_sync` uses
stratum 1 precisely so that the handler's `reference_id` is observable through a real client.

## Test Cases

### 1. `test_ntp_basic_query`

Stratum 2, poll 6. `rsntp` must synchronize. Asserts the stratum reaches the client, the leap
indicator is `NoWarning`, the measured clock offset against our own clock is under 5s (the server
answers from this machine's clock, so a constant or epoch-confused timestamp fails here even though
the packet parsed), and the loopback round-trip delay is in range.

### 2. `test_ntp_time_sync`

Stratum 1 with `reference_id: "GPS."`. Asserts `stratum() == 1`, `reference_identifier() == "GPS."`,
and that the server's reported time is within 5s of ours.

### 3. `test_ntp_stratum_levels`

Raw UDP, two requests on one server.

The v3 request carries a transmit timestamp whose fraction is `0xDEADBEEF` — deliberately non-zero,
so a server copying only the seconds would fail the echo assertion. The reply must be exactly 48
bytes and must match, field for field:

| Field | Bytes | Asserted |
|---|---|---|
| leap indicator | 0 bits 7-6 | 1 |
| version | 0 bits 5-3 | 3 — echoed from the request, not hardcoded 4 |
| mode | 0 bits 2-0 | 4 |
| stratum | 1 | 3 |
| poll | 2 | 10 |
| precision | 3 | -18, read as **signed**; an unsigned round-trip reads 238 |
| root delay | 4-7 | 0.5s as 16.16 fixed point = `0x00008000` |
| root dispersion | 8-11 | 0.25s = `0x00004000` |
| reference id | 12-15 | `LOCL` |
| reference/receive/transmit | 16-23, 32-47 | decode to within 300s of now; receive ≤ transmit |
| origin | 24-31 | the request's transmit timestamp, all 64 bits |

The v4 request is answered with `ignore_request`, and the test asserts **nothing** arrives within
5s. This is the fail-open check: a protocol that answered anyway when the model told it not to would
be indistinguishable from one that works.

Both halves were mutation-checked — changing `reference_id` in the mock, and replacing
`ignore_request` with a time response, each fail the test.

## Known Limitations of the Implementation

Findings, not test bugs:

- **One-second resolution.** `NtpProtocol::get_current_ntp_time` builds timestamps from
  `Duration::as_secs()` and leaves the 32-bit fraction zero, so every reference/receive/transmit
  timestamp ends in `00000000`. Clients accept it; it costs accuracy, not compatibility. The test
  checks only the seconds half and says so.
- **Connections are never reaped.** Each datagram adds a `ConnectionState` to the server instance.
- **Mode is not enforced.** `client_mode` is reported to the model but a mode 4 or 5 packet is
  answered as if it were a client query.

## Not Covered

Stratum 16 (unsynchronized), NTPv1/v2 requests, extension fields and authentication
MACs (`bytes_received > 48` is reported to the model but never exercised), the `send_ntp_response`
raw-hex action, and script-mode handling.

## Expected Runtime

~6s for all three tests against the mock harness. With `--use-ollama`, add one model round-trip per
LLM call (7 total).

## References

- [RFC 5905: NTPv4](https://datatracker.ietf.org/doc/html/rfc5905)
- [RFC 4330: SNTPv4](https://datatracker.ietf.org/doc/html/rfc4330)
- [rsntp](https://docs.rs/rsntp/latest/rsntp/)

## `decision_tag_test.rs`

`test_ntp_llm_failure_is_tagged_fail_closed_and_denies_on_the_wire` — 1 LLM call (the startup
instruction); the `ntp_request` event has no rule, so the mock answers 500 and `call_llm`
returns `Err`.

It asserts **both** halves of NTP's failure contract:

- the wire carries a **Kiss-o'-Death** — LI 3, stratum 0, the client's transmit timestamp
  echoed — which no client will take time from;
- the log carries `decision=fail_closed_llm_error`;
- the log does **not** carry `decision=static_default_llm_error`, the token that named the
  old behaviour where a backend outage still answered with a usable stratum-2 time sample.

The stratum assertion is the substantive one. `fail_closed_*` is a claim that the peer was
denied, and `grep decision=fail_closed` is only worth running if that claim is true — so a
change back to an affirmative answer fails on the wire before it reaches the token.

**This file previously asserted the exact opposite**, and its own header said that if the
behaviour ever became a KoD the stratum assertion would fail first so the token had to be
revisited in the same change. That is what happened: `build_kod_packet` was wired up and the
fail-open closed. `llm_failure_test.rs` asserts the same pair from the other direction
(including the `INIT` kiss code), so neither the wire nor the token can move alone.

See `src/server/ntp/CLAUDE.md`, "Failure behaviour".
