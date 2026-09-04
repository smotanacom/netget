# TUN/TAP tests — strategy, and what they do *not* prove

```bash
./cargo-isolated.sh test --no-default-features --features tuntap \
    --test server::tuntap::e2e_test -- --test-threads=100
./cargo-isolated.sh test --no-default-features --features tuntap \
    --test server::tuntap::packet_test -- --test-threads=100
```

48 tests, all passing, none `#[ignore]`d, none gated on a binary being installed.

## The constraint that shapes everything here

**Creating a TUN or TAP interface needs root on every platform**, and the suite does not have
it. So the transport can never run. The whole point of splitting `packet.rs` (no I/O at all)
from the transport is that everything which decides *what NetGet says and to whom* is testable
without it.

Two consequences, both deliberate:

* `packet_test.rs` drives the decoder, builder and filter against **literal packet bytes**
  written out by hand — real IPv4/IPv6/TCP/UDP/ICMP/ICMPv6 headers with each field commented.
  Nothing in the fixtures is produced by the code under test, so a decoder and builder that
  agree with each other but not with the wire cannot pass.
* `e2e_test.rs` drives the *whole pipeline* over `TunTapEngine::spawn_over_channels`, which
  runs the same `run()` the real device calls with the file descriptor replaced by two `mpsc`
  ends. Frames go in one side, the entire decision path runs, and what NetGet chose to write
  comes out the other.

**Never proven here:** `tun::create()`, the blocking read/write threads, the stop signal, and
whether a real host accepts the packets NetGet builds. Those need `sudo`. See the maturity
section of `src/server/tuntap/CLAUDE.md`.

## The one test that matters most

`the_escalation_bound_holds_under_a_flood`. The design problem this protocol exists to solve is
that a per-packet LLM call is unusable, so "only a bounded few packets reach the model" has to
be **measured**, not asserted in a comment.

It is measured the way `tests/empty_static_handler_test.rs` measures its claim: point NetGet at
a mock model that records every call, then count. Forty packets are injected — twenty TCP,
twenty pings — against `packet_filter: "icmp"` and `llm_max_per_minute: 3`:

| counter | expected | why |
|---|---|---|
| `received` | 40 | everything arrived |
| `filtered_out` | 20 | gate 1 absorbed every TCP packet in native code |
| `escalated_to_llm` | 3 | gate 3's ceiling |
| `dropped_over_budget` | 17 | over the ceiling is dropped, not queued |
| **mock call count** | **3** | the number that actually matters |

**`the_model_is_consulted_when_nothing_else_answers` is the negative control**, and without it
every zero in this file would be indistinguishable from a mock the pipeline never tried to
reach. It must be non-zero. Read the two together or neither means anything — the same rule
`empty_static_handler_test` states about its own control.

## LLM call budget

Four model calls across the whole suite, and every one is deliberate:

| test | calls |
|---|---|
| `the_escalation_bound_holds_under_a_flood` | 3 (the ceiling under test) |
| `the_model_is_consulted_when_nothing_else_answers` | 1 (the control) |
| `an_unbuildable_answer_writes_nothing` | 1 |
| `a_layer_two_answer_on_a_layer_three_interface_is_refused` | 1 |
| everything else | **0**, and that zero is the assertion |

Well under the ~10 the root `CLAUDE.md` asks for, and mostly because this protocol's whole
design is about *not* calling the model.

## Mock expectations

`MockOllamaServer` + `MockLlmBuilder`, in process. Every test that starts a mock finishes with
`wait_for_expectations(30)` then `verify_calls()` — the raw-`MockOllamaServer` spelling of
`wait_for_mocks(30)` + `verify_mocks()`. Without the second one a test asserts nothing about
LLM interaction.

Two things worth knowing before editing them:

* **`respond_with_actions_from_event` is mandatory for the echo replies.** An ICMP echo reply
  that does not carry the request's own `icmp_id` and `icmp_sequence` is discarded by the
  sender, so a hardcoded mock would produce a reply that decodes fine and means nothing. This
  is the same rule `tests/server/dns/CLAUDE.md` states for query IDs, and it is *asserted*
  here: `assert_echo_reply` checks the id and sequence survived the whole pipeline.
* **The lifecycle events are answered by static rules in most tests** (`lifecycle_handled()`),
  so the LLM call counts are about *packets*. That is not a workaround — it is gate 2 working,
  and `escalation_never_spends_no_llm_budget_at_all` deliberately omits them to prove that
  `llm_escalation: "never"` covers the lifecycle events too.

## Waiting

`Harness::wait_for_received` polls the `received` counter rather than sleeping a fixed
interval. The pipeline finishes a frame when it has decided about it, so the counter *is* the
condition being waited on. The fixed-sleep-then-assert shape is exactly the load flakiness the
root `CLAUDE.md` records — enough alone, not enough at `--test-threads=100`.

The one remaining sleep is a 120 ms settle after the last frame is counted, because the counter
is bumped before the decision is made. If that ever proves marginal, count decisions instead of
arrivals rather than lengthening the sleep.

## The off-by-four has its own test, from both sides

`a_packet_information_mismatch_is_an_error_in_both_directions`. Reading a prefixed frame as
bare, and a bare frame as prefixed, must both be **errors**. This is not pedantry: the failure
mode of getting it wrong is not a misaligned header, it is *every decoded field being quietly
wrong*, which is why it costs a debugging pass every time. `the_prefix_written_back_matches_the_one_read`
pins the actual byte layouts (`00 00 00 02` for macOS IPv4, `00 00 08 00` for Linux IPv4, and
the IPv6 forms) and round-trips prepend/strip.

## What is deliberately not tested

* **Checksums against a real stack.** The tests verify each checksummed span sums to zero,
  which is the property a receiving stack checks — but only a real host actually checking it
  proves the pseudo-headers are right. That is the Beta gate.
* **IPv6 extension headers.** The decoder reports the *next header* it finds and does not walk
  a chain, and says so in the source. A packet carrying extension headers decodes as that
  protocol with no ports, which is honest; pretending to have parsed a chain that was never
  implemented would be worse.
* **The `tun` crate itself.** Its own normalisation of the platform prefix is trusted (and the
  reasoning is recorded where `packet_information: "auto"` is documented), not re-tested here.
