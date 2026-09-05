# CDP Test Strategy

Two files, and the split carries the whole argument about what this protocol is
allowed to claim.

| File | Tests | What it proves |
|---|---|---|
| `codec_test.rs` | 19 | The wire format, against literal specification bytes and real captures. No server, no LLM, no socket. |
| `e2e_test.rs` | 3 | Event → LLM → action → frame, end to end, over the declared UDP test transport. |

**22 passing, 0 failing, 0 ignored** at
`--no-default-features --features cdp --test-threads=100`.

Nothing here is `#[ignore]`d and nothing skips. Root `CLAUDE.md` lists four
protocols held at Experimental by a test that prints `SKIP: … not installed` and
returns `Ok(())`; that shape is not used here.

## Running

The test binary is `server`, so the filter is a positional argument:

```bash
./cargo-isolated.sh test --no-default-features --features cdp --test server cdp:: -- --test-threads=100
```

`--test server::cdp::e2e_test` does **not** work — `--test` names a binary, and
`tests/server.rs` is the only one. It fails by listing every test target rather
than by erroring, which is easy to misread as a build problem.

## Why the codec gets its own file

The real CDP transport is raw 802.3 through libpcap and needs packet-capture
privilege. If the wire format lived inside the transport, none of it would be
testable here and the protocol's only evidence would be "it compiles" — the
`wireguard` situation root `CLAUDE.md` records.

So `src/server/cdp/codec.rs` does no I/O at all, and `codec_test.rs` can hammer
it. The evidence is deliberately of two kinds.

### 1. Encode against literal bytes

`encodes_a_full_advertisement_byte_for_byte` builds a complete advertisement
through the public API and compares it, byte for byte, against a frame written
out by hand from the specification — every TLV, the header, and the checksum
literal `0x28a1`.

That literal comes from an **independent transcription of Wireshark's
`packet-cdp.c`**, not from this codec. Asserting a value our own code produced
would prove only that the code is deterministic.

The reference payload happens to be 161 bytes — odd — so this test also runs the
Cisco odd-length checksum branch, which is the branch most CDP implementations
get wrong.

`encodes_the_802_3_and_llc_snap_header_byte_for_byte` pins the framing: the
`01:00:0c:cc:cc:cc` destination, the source MAC, the 802.3 length field covering
LLC + SNAP + payload (a *length*, not an EtherType), and the eight-byte
`AA AA 03 00 00 0C 20 00` header.

### 2. Decode real captures

Four vectors, taken verbatim from **scapy's** CDP regression suite
(`test/contrib/cdp.uts`). Scapy is an independent implementation, so the field
values and checksum verdicts asserted here are not this codec marking its own
homework — the circularity root `CLAUDE.md` names as a failure mode, and the
reason `rss` was demoted (its test parsed the `rss` crate's output with the `rss`
crate).

| Vector | What it is | Used for |
|---|---|---|
| `CAPTURE_CATALYST_2950` | 450-byte CDPv2 from a Catalyst 2950 running IOS 12.1(22)EA14 | full field decode; valid checksum |
| `CAPTURE_IP_PHONE_7960` | 116-byte CDPv2 from a Cisco 7960 IP phone | a **bad** checksum, see below |
| `CAPTURE_POWER_TLVS` | CDPv2 carrying Power Requested/Available TLVs | surviving TLVs we do not model |
| `CAPTURE_FRAME_ROUTER` | a complete 802.3 frame with LLC/SNAP | framing and TLVs **only** — see the warning below |

**`CAPTURE_FRAME_ROUTER` is framing evidence only, and the test says so.** Unlike
the other three it is assembled by hand inside scapy's suite rather than lifted
from a capture: its 802.3 length field says 384 for a 114-byte body, and its
checksum does not match its own contents. Both facts are asserted explicitly so
nobody later mistakes it for a valid packet and "fixes" the decoder to match it.

**The 7960 vector is the most useful one.** Its checksum field says `0xd7db`
while its contents compute `0xf3f1` — scapy's own rebuild test records the same
disagreement. So this is a real device that shipped a bad checksum, and asserting
both numbers proves `checksum_valid()` is capable of returning `false` rather
than always agreeing with itself.

### Checksum tests specifically

`implements_ciscos_odd_length_checksum_padding` pins three odd-length cases —
last byte below `0x80`, at or above `0x80`, and exactly `0x80` — plus an
even-length control.

**Every odd-length assertion also asserts that the RFC 1071 answer is
different** (`assert_ne!`). Without that, someone "simplifying" the padding to
standard RFC 1071 padding could still pass, and the emitted frames would be
rejected by every real Cisco device while the suite stayed green.

The `0x80` case is the one value where Wireshark and scapy disagree
(`byte & 0x80` vs `byte <= 0x80`). It is pinned so the choice is recorded as a
decision. See `src/server/cdp/CLAUDE.md` for why Wireshark wins.

### Robustness

`a_truncated_advertisement_stops_rather_than_over_reads` cuts a real capture at
five offsets, including mid-TLV. `a_zero_length_tlv_does_not_loop_forever` feeds
a TLV whose declared length is below its own 4-byte header — the shape that would
not advance the cursor. Neither may panic or spin; a capture handle receives
whatever is on the wire, including whatever an attacker puts there.

## The E2E tests

The server declares a `transport: "udp"` startup parameter under which each
datagram is one complete 802.3 CDP frame. That makes the whole chain runnable
unprivileged, with only the pcap read/write calls left unexercised. `ospf` set
the precedent of driving a privileged protocol over UDP in tests; here it is a
declared mode of the protocol rather than a test that quietly runs a different
one.

The neighbour's frame is **built by hand** in the test from the captured Catalyst
payload plus a hand-written 802.3 + LLC/SNAP header, so the input does not depend
on the code under test.

### `test_cdp_answers_a_neighbour_with_the_identity_the_model_chose`

The mock rule matches on `device_id`, `port_id` **and** `platform` in the event
body. That is the assertion, not decoration: if the server handed the model a hex
blob, or named the fields differently, the rule would never fire and
`verify_mocks` would report zero calls for it.

The reply is checked twice over — first as raw bytes (destination MAC, source
MAC, 802.3 length field, the eight SNAP bytes, version, TTL) before any parser is
trusted, then through the codec for the TLV fields the model chose, including
that the checksum it emitted verifies.

### `test_cdp_emits_nothing_when_the_llm_fails`

The mock answers the event with plain text that is not an action, which is what a
broken or hallucinating backend looks like; the retry/repair loop exhausts and
`call_llm` returns `Err`. The test then asserts three things:

1. **No datagram comes back at all.** CDP has no error frame, and its only frame
   asserts that a device exists — a fabricated one would write a fake switch into
   a neighbour's table for `ttl` seconds.
2. The log carries `decision=fail_closed_llm_error`, so the failure is
   distinguishable from a model that chose silence.
3. No line matching `CDP advertisement sent via` appears anywhere.

It waits for the decision line before starting the silence timer, so the silence
being measured is the silence of a *failed* answer rather than of an answer still
in flight — the fixed-sleep mistake root `CLAUDE.md` records.

### `test_cdp_no_advertisement_is_silent_and_logged_as_a_refusal`

The other silence. `no_advertisement` also emits nothing, but must log
`decision=model_reject`. Without this test the two silences would be
indistinguishable, which is exactly what the fail-closed discipline in
`src/server/radius/` exists to prevent.

## LLM call budget

Six mocked calls total across the three tests — one startup call each, plus one
event call each (the failure test's event call retries, hence `expect_at_least`).
Well under the ~10 the root `CLAUDE.md` asks for.

Every test finishes with `wait_for_mocks(30)` then `verify_mocks()`. Without the
latter a test asserts nothing about LLM interaction at all.

## Environment requirement — it fails loudly rather than skipping

The privilege gate in `server_startup.rs` is checked against `metadata()`, which
cannot vary by transport. CDP declares `PacketCapture` (correctly — it is a pcap
protocol, like `arp`/`datalink`/`isis`), so **starting a CDP server at all needs
packet-capture capability, even under the `udp` transport that uses none.**

On this machine `/dev/bpf*` is readable (ChmodBPF), so it is satisfied. On a
runner without it, the e2e tests fail at startup with the privilege message
rather than skipping. That is deliberate: a silent skip is how four protocols in
this repo ended up with maturity ratings resting on nothing.

The codec tests have no such requirement and run anywhere.

## What these tests do NOT prove

- **The raw 802.3 transport.** Never executed. No pcap handle is opened by any
  test.
- **Interoperability with anything.** No real Cisco device, no third-party CDP
  implementation, has ever received a frame from this server. The captures prove
  the *parser* agrees with scapy; they say nothing about what a switch does with
  our output.
- **Periodic advertisement.** There is no 60-second timer to test.

The path to closing the first two is in `src/server/cdp/CLAUDE.md`: this machine
has the `feth` driver, so `sudo ifconfig feth0 create && sudo ifconfig feth1 peer
feth0` gives a real Ethernet pair with no hardware, and Wireshark's own CDP
dissector is the independent check. **Not done, and not claimed.**
