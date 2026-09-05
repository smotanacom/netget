# LLDP Tests

## Strategy: split by what is actually knowable

LLDP's real transport is raw Ethernet and needs `CAP_NET_RAW` / `/dev/bpf*`. Nothing in this
repository has that, and `server_startup` refuses to spawn a protocol whose declared privilege is
unmet — so on any developer machine and any CI runner, the pcap path cannot be executed at all.
The suite is therefore split the way `tests/server/bluetooth_ble_beacon/` is:

| File | Runs everywhere | What it proves |
|---|---|---|
| `codec_test.rs` | yes | The frame format, against literal IEEE 802.1AB bytes |
| `e2e_test.rs` | yes | The whole event → handler/LLM → action → frame path, over the UDP test transport |
| — | — | **Nothing here proves anything about pcap.** See `src/server/lldp/CLAUDE.md`. |

**LLM budget: 1 call.** One test uses a mock Ollama; every other test either uses a static
handler (no call by construction) or points the client at `127.0.0.1:1` so that the outcome
*proves* no call succeeded.

```bash
./cargo-isolated.sh test --no-default-features --features lldp \
    --test server lldp -- --test-threads=100
```

28 tests, ~1.1s.

## `codec_test.rs` — literal bytes, not round-trips

Every expected byte string is written out literally and derived from the published layout, **not
from the implementation**. Round-tripping the encoder through the decoder would prove only that
one function inverts the other, which the root `CLAUDE.md` names as circular evidence; it appears
here exactly once, as the last test in the file, explicitly labelled as a consistency check and
not as the argument.

Sources are named in the file header: IEEE 802.1AB-2016 §8.1, §8.4, §8.5.1–8.5.9 and Tables 7-1,
8-2, 8-3, 8-4.

Coverage worth keeping if these are ever rewritten:

- **The TLV header.** 7-bit type and 9-bit length share the first octet, so a type-4 TLV of
  length 23 is `08 17`, not `04 17`. An implementation that wrote type and length as two separate
  octets fails this rather than passing by luck.
- **The chassis/port subtype asymmetry.** A MAC address is chassis subtype **4** and port subtype
  **3**; a network address is **5** and **4**. This is the single easiest thing to get wrong in
  LLDP and the symptom is a neighbour showing a plausible-looking wrong field, so it is asserted
  both through the lookup tables and in the encoded bytes.
- **`network_address` carries an IANA family octet first.** Subtype 5 is not "an IP address", it
  is "a family octet then an address", and omitting the family shifts everything after it. Both
  IPv4 (family 1) and IPv6 (family 2, eighteen octets) are asserted.
- **Every capability bit separately**, against Table 8-4, plus `reserved_bit_15` for an
  unassigned one — a neighbour that sets it is telling us something — plus the refusal of a
  mistyped name, because silently dropping it would let a model believe it had claimed to be a
  bridge when it advertised nothing.
- **The management address TLV's internal lengths.** `05 01 c0 00 02 0a 02 00000003 00` — an
  address-string length that counts the family octet, then family, address, interface numbering
  subtype, interface number, OID length. Three separate length fields, and only literal bytes
  catch a wrong one.
- **A real-world capture** (`real_world_capture_tlvs_decode`): the Extreme Networks Summit300-48
  frame's chassis/port/TTL/capabilities/management TLVs. **Only the TLVs whose octets were
  re-derived arithmetically from the spec are asserted** — the capture's text TLVs are
  deliberately left out rather than transcribed from memory, which would turn "checked against a
  real capture" into a claim about nothing.
- **Unknown TLVs are skipped, not fatal.** Type 127 appears in every frame a real switch sends; a
  decoder that rejected it would refuse most real traffic. The test puts a System Name *after* an
  organisationally-specific TLV to prove the walk resumed at the right offset.
- **Mandatory TLV order is enforced**, because a decoder that accepted any order would silently
  accept frames a conforming neighbour rejects.
- **No octets anywhere in event data.** `event_data_carries_no_octets_anywhere` checks every key
  and value: no `*_hex`/`*_raw`/`*_bytes` key, no long hex-looking string. This is the rule the
  whole protocol design hangs on and it is worth a test rather than a comment.

## `e2e_test.rs` — in-process, over the UDP transport

Builds a real `SpawnContext` and calls `Server::spawn` directly, because the child-process
harness cannot start LLDP unprivileged (the `RawSockets` gate fires before `spawn`). The server
is asked for its declared `transport: "udp"`, which carries complete Ethernet frames as datagram
payloads — so the codec, the event, the handler/LLM dispatch, the action executor and the frame
builder all really run, and the test decodes what comes back with the same codec a neighbour
would use.

`state.set_ollama_model(Some(...))` is set before spawning: without it `ensure_model_selected`
tries to auto-select against `localhost:11434` and the test would depend on the developer's
machine.

### The tests, and why each exists

| Test | The point |
|---|---|
| `an_identity_the_model_authors_reaches_the_wire` | The protocol's whole purpose. A mock model chooses a chassis ID, a Cisco system description and a capability split, and every one of those lands in the decoded frame. Also asserts the **source MAC comes from the server**, not the model. |
| `a_static_handler_advertises_with_no_llm_call` | The deterministic path, with the LLM endpoint unreachable — a frame arriving proves no call was *needed*, not merely that none was counted. |
| `the_advertise_timer_announces_us_unprompted` | `lldp_advertise_due` really fires. An event declared and never raised is a defect this repo has shipped in bulk. |
| `an_llm_failure_advertises_nothing` | The reason the protocol is in the deliberately-silent class. Asserts the `decision=fail_closed_*` tag **and** that no datagram followed. |
| `no_backend_error_text_can_reach_a_neighbour` | The other half of the `WireFailure` rule, asserted as the stronger property: nothing is sent, so there is nothing for an error to hide in. |
| `no_policy_means_no_frame_and_no_llm_call` | The default is a listener, and it costs no LLM round-trip per frame. |
| `a_deliberate_refusal_is_logged_as_a_decision` | `no_advertisement` must be distinguishable from silence **in the log**, since on the wire it is not. |
| `unusable_startup_parameters_are_refused` | Five combinations, including `udp_peer` with `transport: "raw"`, where it would do nothing. |
| `an_undeclared_parameter_names_the_declared_ones` | The error lists what is available, so a model can correct itself. |
| `the_raw_transport_refuses_rather_than_pretending` | The ARP/DataLink/ICMP/IS-IS defect — a server in `Running` having captured nothing. Fixed four separate times elsewhere. |

### Timing

No fixed sleeps. `wait_for_status` polls the status stream against a deadline, and every socket
read is a `tokio::time::timeout`. The "nothing was sent" assertions use a **non-blocking**
`try_recv_from` and are made *after* the decision has been observed in the log, so they are not
racing a frame still in flight. Under `--test-threads=100` the whole file finishes in about a
second.

## What still has no coverage

Everything past the codec on the real transport:

- No frame this code produced has reached a real LLDP neighbour.
- `pcap::Capture::open`, the BPF filter compile, `sendpacket` and the capture loop have never
  been executed. Their error paths are asserted only through the "no such device" case, which
  fails before the privileged step.
- The self-frame filter (ignoring our own injected advertisement, captured back off the wire)
  is exercised only in principle — on the UDP transport it cannot happen.

A green run here means "the bytes are right and the failure discipline is honest". It does not
mean LLDP works. `src/server/lldp/CLAUDE.md` records the `feth`-pair + `lldpd` experiment that
would change that, and it has not been run.
