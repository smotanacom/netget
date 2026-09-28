# NSQ tests

Run everything:

```bash
./cargo-isolated.sh test --no-default-features --features nsq --test server -- nsq:: --test-threads=100
```

## Strategy

The evidence is `real_client_test.rs`, which points the NSQ project's own `to_nsq` and
`nsq_tail` (go-nsq) at the server and asserts on what they printed and how they exited.
Everything else is NetGet reading bytes NetGet wrote. Wireshark has no NSQ dissector, so there is
no pcap oracle.

Most suites start the server **in process** through `ServerForm` with static, Python script or
`manual` handlers and a dead model endpoint. `e2e_test.rs` and one case in `real_client_test.rs`
use the spawned binary with the mock model.

## Files

| File | What it proves | Model calls |
|---|---|---|
| `common.rs` | helpers: in-process server, a raw V2 peer (commands via `wire::encode_command`, frames via `wire::parse_frame`, heartbeats skipped unless asked for), `BROKER_SCRIPT` (accept every publish except to `refused`, accept every SUB, deliver `hello`/`world` on topic `greetings` and `<topic>-1` elsewhere) | — |
| `real_client_test.rs` | `to_nsq` exits 0 after two stdin lines, each a PUB the handler accepted; to topic `refused` it exits non-zero naming `E_PUB_FAILED`; `nsq_tail -n 2` prints exactly `hello\nworld\n` from one RDY; and a **mocked model** sees `to_nsq`'s two lines in `nsq_publish` and delivers them to `nsq_tail`, which prints them. Fails, never skips, without the binaries. | 5 (mocked case) |
| `e2e_test.rs` | mocked model: IDENTIFY without and with feature negotiation; PUB and MPUB summaries exactly as sent; CLS before SUB refused and closed; SUB; RDY 2 against three delivered messages (two arrive, the third follows the first FIN); TOUCH and FIN of unknown ids refused without closing; REQ answered by a redelivery with attempts 2 under a new id; CLS → `CLOSE_WAIT`, and RDY after it raises nothing | 9 |
| `wire_test.rs` | every command from its wire form; every prefix of a command asks for more bytes (the loop re-parses after a heartbeat wins a race); nsqd's error codes and texts, sizes and MPUB counts judged from the size fields; name rules; frames and messages read back; IDENTIFY's heartbeat rules and negotiation reply; proptests: any command round-trips with trailing bytes left alone, any message frame round-trips, the parsers never panic | 0 |
| `connection_bounds_test.rs` | a 1 MiB PUB accepted and a declared 1 MiB + 1 refused with 48 KiB of unread body behind it, then EOF, and no handler; MPUB over 5 MiB and over the count bound refused from their leading bytes; a 1024-byte line read and a newline-less flood refused; a wrong magic → `E_BAD_PROTOCOL`; first-byte deadline; a client answering heartbeats with NOP held for 5 s at a 1 s interval, one silent after IDENTIFY closed after two, one that disabled heartbeats held to the idle bound; a publish parked for a human keeps receiving heartbeats and is not closed; 1001 messages before RDY → 1000 kept, one dropped, and RDY 3, one FIN and RDY 2500 release exactly 3, 1 and the other 996; the 257th connection closed with no bytes, slot returns | 0 |
| `llm_failure_test.rs` | dead backend → `E_PUB_FAILED PUB failed: broker backend …`, `E_MPUB_FAILED MPUB failed: …`, `E_INVALID SUB failed: …`, each closing, no leaked error text, `decision=fail_closed_llm_error`; on RDY nothing is delivered and the connection stays; empty handler → `E_PUB_FAILED PUB failed` + `model_silent`; a model refusal sent + `model_reject`; the first reply wins; a model `E_FIN_FAILED` keeps the connection | 0 |
| `peer_inject_test.rs` | `send_to_peer` with `deliver_nsq_messages` goes through the RDY limit (RDY 1: one arrives, the second after FIN); `close_connection` reaches the client as EOF | 0 |
| `answer_with_test.rs` | the `answer_with` hints name the one answer, the refusal code, the room and where bodies come from; every event carries one; examples are placeholders | 0 |

## How each guard was shown to matter

Removed in three mutated builds, then restored:

| Guard removed | Test that failed |
|---|---|
| PUB size check | `a_message_of_exactly_the_limit…` (no answer), `refusals_use_nsqds_codes…` |
| MPUB body-size check | `an_mpub_past_the_body_bound…` |
| MPUB early count check | `an_mpub_past_the_body_bound…` (waited for a body that never came) |
| line bound | `a_line_of_the_limit…` |
| magic check | `anything_but_the_v2_magic…` |
| first-byte deadline (→ 3600 s) | `a_peer_that_says_nothing…` |
| heartbeat silence (→ the idle bound) | `a_client_silent_for_two_heartbeats…` |
| idle bound without heartbeats (→ 3600 s) | `a_client_that_disabled_heartbeats…` |
| heartbeats during a model call | `a_publish_parked_for_a_human…` |
| RDY limit | `deliveries_past_rdy_wait…`, `e2e`, `peer_inject` |
| pending bound | `deliveries_past_rdy_wait…` |
| connection cap (→ 100 000) | `the_connection_past_the_cap…` |
| the closing refusal for a silent publish (→ `OK`) | `a_handler_that_answers_nothing…` |
| nsqd's fatal/non-fatal split (→ all fatal) | `a_non_fatal_refusal_of_a_fin…`, `e2e` |
| `linger` | `a_message_of_exactly_the_limit…`: `ConnectionReset` |

## Notes

- `to_nsq`/`nsq_tail` are at `/opt/homebrew/bin` here (`brew install nsq`); CI's `registry-audit`
  unpacks the v1.3.0 release tarball into `/usr/local` and runs `nsq::real_client_test`.
- `nsq_tail -n N` caps its in-flight count at N and exits from inside the handler of the N-th
  message, before its FINs are flushed, so no test depends on a FIN from it.
- `to_nsq` logs `exiting router` only on a clean stop; a refused PUB is fatal to it.
