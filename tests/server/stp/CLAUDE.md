# STP / RSTP test strategy

Two files with different jobs, and the split is the whole strategy:

| File | Proves | Needs |
|---|---|---|
| `codec_test.rs` | the BPDU codec against **literal specification bytes**, both directions | nothing — no socket, no LLM, no privilege |
| `e2e_test.rs` | the full frame → event → model → action → frame path, the silence guarantee, and startup-parameter handling | a mock Ollama on loopback |

27 tests, all passing, ~5 s at `--test-threads=100`.

```bash
./cargo-isolated.sh test --no-default-features --features stp \
    --test server -- stp:: --test-threads=100
```

**LLM call budget: 3.** One per exchange test
(`a_bpdu_produces_the_bpdu_the_model_decided_on`,
`a_topology_change_notification_raises_the_topology_change_event`,
`no_bpdu_transmits_nothing_and_is_logged_as_a_decision`). The silence test makes
zero calls reach a mock *by design* — its backend is a closed port.

## Why the literals, and why they are not circular

Encoding with our encoder and decoding with our decoder proves only that the two
agree with each other. The root `CLAUDE.md` names that as circular evidence, and
it is exactly the mistake that held `rss` at Experimental while its test
round-tripped one crate through itself.

There is no third-party STP codec in this tree and no STP peer that can be run
here, so the external reference is **the specification's field table, written out
by hand as octets**.

**Provenance, stated plainly:** every byte string in `codec_test.rs` was assembled
by hand from IEEE 802.1D-2004 §9.3.1 (configuration BPDU), §9.3.2 (topology change
notification) and 802.1w §9.3.3 (RST BPDU), using the default parameter values the
standard recommends in Table 17-1. They are **not** extracted from a named packet
capture, and the test does not claim they are. What makes them evidence is that
the offsets, byte order, timer scaling and priority split were written out here
independently of the encoder — an encoder that gets any of them wrong disagrees
with this file rather than with itself.

If you can get a real capture, replacing these with bytes from one is a strict
improvement and worth doing. Say where they came from when you do.

### The three literal frames

All three are complete 802.3 frames padded to the 60-octet Ethernet minimum, so
the framing, the LLC header, the length field and the padding are pinned too — not
just the BPDU body.

* **`CONFIG_BPDU_FRAME`** — 802.1D configuration BPDU, version 0, type 0x00, flags
  0x00, root and bridge both priority 32768 / VLAN 0 / `00:1c:0e:87:78:00`, port
  id `0x8004`, the recommended timers. Length field `0x0026` = 38 = LLC(3) +
  BPDU(35).
* **`RST_BPDU_FRAME`** — 802.1w RST BPDU, version 2, type 0x02, flags **0x3c**
  (designated + learning + forwarding — the byte a converged RSTP port sends),
  identifiers on **VLAN 1** so the leading octets are `0x8001`, and the trailing
  version-1-length octet. Length field `0x0027` = 39 = LLC(3) + BPDU(36).
* **`TCN_BPDU_FRAME`** — four octets of content, length field `0x0007`.

The same `CONFIG_BPDU_FRAME` and `TCN_BPDU_FRAME` literals are re-declared in
`e2e_test.rs` and sent over the socket, so what crosses the wire in the e2e tests
is independently pinned bytes rather than whatever the encoder produced that day.

### The two assertions worth keeping

**1/256-second timers.** `timers_are_encoded_in_units_of_one_256th_of_a_second`
asserts `0x1400` / `0x0200` / `0x0f00` at the four timer offsets, *and* asserts
they are **not** `20` / `2` / `15`. The negative half is what makes a failure
legible: without it a regression reports `5120 != 20` and the reader has to work
out which side is wrong. `the_timer_unit_has_1_256th_second_resolution_in_both_
directions` covers the fractional case (0.5 s = 128 ticks), the 16-bit ceiling
(256 s must be refused, not wrapped) and negatives.

**Priority / system-ID-extension packing.**
`bridge_priority_and_system_id_extension_share_one_16_bit_field` walks the
corners: `0x8000` (32768/VLAN 0), `0x8001` (32768/**VLAN 1** — the value people
misread as "32769"), `0x0000` (priority 0, claiming the root of the whole tree),
`0xffff` (61440/4095) and `0x1064` (4096/100).
`a_priority_that_is_not_a_multiple_of_4096_is_refused` asserts 32769 is rejected
**and** that the error explains the low bits are the system ID extension — a
refusal that does not say why trains people to work around it.
`port_priority_and_port_number_share_one_16_bit_field` does the same for the 4/12
port identifier.

Both of these caught a real bug while being written: the first draft of the
maximum-value cases expected `[0xf0, 0xff]` for priority 61440 on VLAN 4095. The
correct answer is `0xf000 | 0x0fff` = `[0xff, 0xff]`. The **test** was wrong and
the code was right, which is the direction this file is supposed to fail in.

### The rest of `codec_test.rs`

* every flags bit at its 802.1w position, encode and decode, including the
  composite `0x3c`;
* the 802.3 length field counting LLC + BPDU and excluding padding, on all three
  frames;
* the STP LLC header (`42/42/03`) being required — a SNAP frame (`aa/aa/03`, which
  is CDP's encapsulation) and an Ethernet II frame (an EtherType where 802.3 has a
  length) are both refused with an error that names the reason;
* truncated frames refused rather than read past, including a frame whose length
  field claims more than arrived;
* an unknown BPDU type reported rather than guessed, and a non-zero protocol
  identifier refused;
* a 4-octet root path cost (200 000, the 802.1D-2004 cost of a 10 Mb/s link, which
  does not fit 16 bits) surviving the round trip.

## Why `e2e_test.rs` does not use the harness

Every other server suite starts the `netget` binary and lets `server_startup`
bring the protocol up. **That cannot work here**, and the reason is worth knowing
before you try:

`server_startup`'s privilege gate is **per-protocol, not per-transport**. STP
declares `PrivilegeRequirement::RawSockets`, and `requires_privileges` is
`!privilege_met` for that variant, evaluated *before* the startup parameters are
read. So an unprivileged `start_server` is refused even with `transport: "udp"`,
which needs no privilege at all. Declaring anything weaker would be a lie about
the raw transport, which is the real one.

So these tests build a `SpawnContext` by hand and call `Server::spawn(ctx)`
directly. That still exercises everything this protocol owns — startup-parameter
parsing, the bind, the 802.3 decode, the event, the dispatch, action execution,
the re-encode and the transmit. Only `server_startup`'s own gate is bypassed, and
that gate is not this protocol's code.

The harness pieces that *are* used directly are `MockOllamaServer` and
`MockLlmBuilder`, the same way `tests/empty_static_handler_test.rs` uses them. The
state is built with `AppState::new_with_options(false, mock.base_url())` plus
`set_llm_client`, and **`set_ollama_model(Some("mock-model"))`** — without pinning
the model, `ensure_model_selected` falls back to probing a real Ollama on
`localhost:11434` and the test starts depending on the developer's machine.

`wait_for_expectations(30)` then `verify_calls()` on every test that uses a mock,
per the root `CLAUDE.md`: waiting on the expectations waits on the exchange, and
`verify_calls` is the thing that actually asserts.

## The UDP transport in tests

One **complete 802.3 frame per datagram** — the same octets the raw transport
would put on the segment, with only the link layer simulated. The reply comes back
to the datagram's sender. So the codec, the framing, the event, the dispatch, the
action and the response encoding are all the real ones.

The bridge is configured with `bridge_priority: 4096`, deliberately **not** the
default 32768, so an assertion on it proves the startup parameter was read rather
than that a constant happened to match.

## What each e2e test is for

**`a_bpdu_produces_the_bpdu_the_model_decided_on`** — the whole path. The model is
told to claim the root bridge with priority 0, which is the actual attack this
protocol makes possible, and the reply frame is decoded and asserted field by
field: root priority really is 0, the port role really is designated, the source
MAC comes from `bridge_mac`, and every field the action *omitted* (bridge
priority, port id, all three timers) came from the startup parameters rather than
from a constant. That last group is the `ospf` defect the root `CLAUDE.md`
records — four of its six parameters were advertised and reached the wire from
nowhere.

**`a_topology_change_notification_raises_the_topology_change_event`** — the event
split. A TCN must raise `stp_topology_change`, not `stp_bpdu_received`; routing it
to the wrong event would make an operator's handler for it never match, silently.

**`no_bpdu_transmits_nothing_and_is_logged_as_a_decision`** — an explicit refusal
produces no frame **and** a `decision=model_reject` line. On the wire this is
identical to every other silence, so the tag is the only place the difference
survives.

**`an_llm_failure_puts_nothing_on_the_wire`** — the one that matters most. The
backend is `http://127.0.0.1:1`, a closed port.

A bare "no frame arrived" would prove nothing: it is equally consistent with a
server that never received the frame, which is the shape of assertion the root
`CLAUDE.md` warns about under the empty-static-handler investigation. So this
asserts a **pair**: the `decision=fail_closed_` status line proves the frame was
decoded, the event raised and the model asked; the absent frame proves the failure
produced no output. `a_bpdu_produces_the_bpdu_the_model_decided_on` is the
positive control for the same path.

Budget: 90 s for the decision line, because the LLM client retries before giving
up, then 5 s to confirm nothing arrived. The order matters — the no-frame check
runs *after* the decision is known to have been made.

**`the_declared_startup_parameters_are_exactly_the_ones_the_server_reads`** — a
local echo of `tests/startup_param_drift_test.rs`, pinning the set to the ten
`StpBridgeConfig::from_startup_params` reads, plus a check that an undeclared key
is refused by name.

**`an_unencodable_bridge_priority_refuses_to_start`** — `bridge_priority: 32769`
must fail at `spawn()`, naming the 4096 step, rather than starting a bridge that
fails on every BPDU it later tries to send.

**`the_raw_transport_never_reports_success_without_a_capture_handle`** — the
ARP/DataLink/ICMP/IS-IS regression guard, done locally because the shared
`tests/capture_startup_reports_failure_test.rs` is not this protocol's file to
edit.

Two branches:

* a device that does not exist → **always** `Err`, whatever the privilege, because
  the lookup happens before the open. The error must name the device.
* loopback **without** capture privilege → `Err` naming `/dev/bpf` or
  `CAP_NET_RAW`. This is the branch every developer machine and CI runner takes.

The loopback branch is **skipped when the host has capture access**, and that is
not laziness: Linux presents `lo` with a synthetic Ethernet header, so
`ether dst 01:80:c2:00:00:00` compiles there and a privileged spawn legitimately
succeeds. macOS `lo0` is `DLT_NULL` and the filter is rejected. An assertion that
held on both would have to encode that difference, and it would be asserting
libpcap's behaviour rather than netget's.

## What these tests do NOT prove

* **The raw 802.3 transport has never been executed.** No test here runs
  privileged. The pcap open, the BPF filter, the injection thread and the
  cooperative stop are untested code.
* **No third-party STP peer has ever spoken to this server.** Not `mstpd`, not a
  switch, not anything. The codec is checked against the spec; nothing has checked
  it against another implementation's *reading* of the spec.

That is why the protocol is `Experimental`, and the codec tests are not grounds to
promote it. See `src/server/stp/CLAUDE.md` for the `feth`-pair recipe that would
give a real Ethernet segment on this machine — **nobody has run it.**

## Adding a test here

Reuse the literals. If you need a new frame shape, write the bytes out by hand
from the specification with an offset table in a comment, the way the three
existing ones are — do **not** generate it with `encode_frame` and paste the
result, which reintroduces exactly the circularity this file exists to avoid.
