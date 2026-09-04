# STP / RSTP Protocol Implementation

Spanning Tree (IEEE 802.1D-2004) and Rapid Spanning Tree (802.1w) bridge. NetGet
receives BPDUs off an Ethernet segment, decodes them into structured fields, and
transmits whatever BPDU the operator's handler or the model decides on.

**State**: `Experimental`. **Privilege**: `RawSockets`. **Connectionless**: yes.
**Stack**: `ETH>LLC>STP`.

## The point, and the hazard

**The model chooses the bridge priority.** That is the whole reason this protocol
is interesting and the whole reason it is dangerous.

A configuration BPDU carries a *root identifier*, and the bridge advertising the
numerically lowest one wins the root election. Every switch that believes it
recomputes its spanning tree: ports change role, some stop forwarding for the
duration of the forward delay, and traffic that was crossing the segment stops
while they do. A `bridge_priority` of 0 does not "join" a network — it takes over
its topology. On a real switched LAN that is an outage, and on a LAN with the
attacker's bridge in the middle it is a man-in-the-middle position.

This is not a hypothetical property of a badly-behaved model. It is one number in
one action:

```json
{"type": "send_stp_bpdu", "root_priority": 0, "root_path_cost": 0}
```

Point this at a lab segment. `tests/server/stp/CLAUDE.md` describes the isolated
`feth` pair to use.

## Why silence is the failure mode

STP is in the **deliberately-silent** class the root `CLAUDE.md` catalogues, and
its case is among the strongest there.

Every BPDU this server can emit is a *positive assertion about topology*. A
configuration BPDU says "the root is X and my cost to it is Y". A TCN says "the
topology changed". There is no error BPDU, no NAK, no "I do not know" — the
protocol has three message types and all three are claims. So when the LLM call
fails there is nothing truthful to send, and sending something anyway would not
merely mislead a peer: it would re-converge a real network segment on a claim
NetGet cannot back.

The peer already handles our silence correctly. Its own max-age timer ages out a
bridge that stops speaking; that is the spec-defined outcome for "that bridge is
gone", and it is exactly right.

So the LLM-failure path in `dispatch_event` writes **nothing** to the wire and
records the failure in the log, the way `src/server/radius/` keeps its cases
apart. Nothing derived from the error reaches a frame — the error is classified
with `crate::utils::WireFailure` only to keep an overloaded backend
distinguishable from a broken one in the operator's log.

| `decision=` | Meaning |
|---|---|
| `no_policy` | No instruction and no handler configured. Observed passively, **no LLM call at all**. |
| `model_reject` | The model chose `no_bpdu`. A real answer: observe, transmit nothing. |
| `model_silent` | The model returned no usable action. Not a decision. |
| `model_invalid` | The action could not be encoded (e.g. a priority that is not a multiple of 4096). Nothing transmitted. |
| `fail_closed_overloaded` | The LLM backend was saturated. Nothing transmitted. |
| `fail_closed_llm_error` | The LLM call failed some other way. Nothing transmitted. |

All six look identical on the wire. The log is the only place the difference
survives, which is why the tags are stable and greppable.

`tests/server/stp/e2e_test.rs::an_llm_failure_puts_nothing_on_the_wire` asserts
the pair that makes this meaningful: the `decision=fail_closed_` line proves the
frame was decoded, the event raised and the model asked, and the absent frame
proves the failure produced no output.

## Architecture: the codec is separate from the transport, and that is the point

You cannot open a raw socket in this environment — nothing here runs as root. So
the protocol is split so that the part which *can* be proved is proved:

```
codec.rs     pure, no I/O, no async, no AppState.  Bytes <-> structured values.
             Proved against literal specification bytes, both directions.
mod.rs       two thin transports over it.
             Raw (pcap): NEVER EXECUTED.
             UDP:        exercised end to end, unprivileged.
actions.rs   what the model sees; startup parameters; action -> codec values.
```

This is the `bluetooth_ble_beacon` precedent: pure construction exhaustively
tested against literal spec bytes, transport never executed, and `metadata().notes`
saying **both**.

### `codec.rs` — what it encodes

A BPDU rides in an **802.3 length-encapsulated** frame with an 802.2 LLC header,
never in an Ethernet II frame:

```
 0.. 6  destination MAC   01:80:C2:00:00:00 (the Bridge Group Address)
 6..12  source MAC
12..14  length            LLC(3) + BPDU length — padding NOT counted
14..17  LLC               DSAP 0x42, SSAP 0x42, UI control 0x03
17..    BPDU              35 (config) / 36 (RST) / 4 (TCN) octets
        padding to the 60-octet Ethernet minimum
```

Configuration BPDU body (802.1D-2004 §9.3.1):

```
 0.. 2  protocol identifier 0x0000     17..25  bridge identifier
 2      version 0=STP 2=RSTP           25..27  port identifier
 3      type 0x00/0x02/0x80            27..29  message age    (1/256 s)
 4      flags                          29..31  max age        (1/256 s)
 5..13  root identifier                31..33  hello time     (1/256 s)
13..17  root path cost                 33..35  forward delay  (1/256 s)
                                       35      version 1 length (RST only, 0x00)
```

**Two things implementations get wrong, and both are pinned to literal bytes.**

1. **Timers are in units of 1/256 second, not seconds.** Max age 20 s is
   `0x1400`, hello time 2 s is `0x0200`, forward delay 15 s is `0x0F00`. Writing
   the number of seconds straight into the field yields `0x0014`, a BPDU claiming
   a 78-millisecond max age — and a real bridge acts on it. The codec exposes
   seconds and does the scaling in one place
   (`seconds_to_ticks` / `ticks_to_seconds`); the test asserts the encoded bytes
   at their offsets *and* asserts they differ from the naive encoding, so a
   regression says what went wrong.

2. **The 16-bit priority field is two fields.** Since 802.1t/802.1Q the high 4
   bits are the bridge priority and the low 12 bits are the *system ID extension*
   — in practice the VLAN id. So priority is only expressible in steps of 4096,
   and `0x8001` is **priority 32768 on VLAN 1**, not "priority 32769". `BridgeId`
   surfaces the two halves as separate fields and **refuses** a priority that is
   not a multiple of 4096, with an error that says why. The port identifier is
   the same shape (4-bit priority in steps of 16, 12-bit port number); 802.1D-1998
   split it 8/8 instead, and the two agree byte-for-byte for the common values —
   `0x8004` is priority 128, port 4 under either reading.

The RSTP flags octet is seven named booleans plus a `PortRole` enum, never a
number the model has to assemble:

```
bit0 0x01 topology change   bit4 0x10 learning
bit1 0x02 proposal          bit5 0x20 forwarding
bit2-3 0x0C port role       bit6 0x40 agreement
       (00 unknown, 01 alternate/backup, 10 root, 11 designated)
                            bit7 0x80 topology change ack
```

`0x3C` — designated, learning, forwarding — is the byte a converged RSTP port
sends, and is one of the literals in the test.

### `mod.rs` — the two transports

**Raw (`transport: "raw"`, the default).** libpcap, promiscuous, filtered to
`ether dst 01:80:c2:00:00:00`. Receive on one handle, inject on a second from a
dedicated thread. `spawn()` awaits a oneshot from the blocking task and returns
`Err` unless the handle *and* its filter are genuinely open — the
ARP/DataLink/ICMP/IS-IS fire-and-forget defect the root `CLAUDE.md` records is
designed out rather than left to be found later. Stopping is cooperative through
`crate::utils::StopSignal`, because `JoinHandle::abort()` cannot interrupt a
thread parked in `next_packet()`.

`ether dst` needs an Ethernet link layer, so the filter is rejected on loopback,
tunnels and raw-IP devices on macOS/BSD. That refusal is correct — there is no
bridged segment there and a spanning tree server on `lo0` would sit in `Running`
having seen nothing. (On Linux, pcap gives `lo` a synthetic Ethernet header and
the filter compiles, so a privileged Linux spawn on `lo` legitimately succeeds
and sees nothing useful. The test accounts for this.)

**UDP (`transport: "udp"`).** One **complete 802.3 frame per datagram** — the
same octets the raw transport would put on the segment, with only the link layer
simulated. Replies go back to the datagram's sender. This exists so the full
frame → event → model → action → frame path runs unprivileged, which is the
compromise `ospf` documents as "requires root/CAP_NET_RAW, tests use UDP", done
inside the protocol instead of by substituting a generic UDP server in the test.

**There is no spanning tree state machine.** Nothing elects a root, ages a timer,
transitions a port or transmits unprompted. NetGet supplies the wire; the model
supplies the decisions. This is deliberate under the no-storage rule — the server
holds no bridge table, no topology, no learned state of any kind.

## What the model sees and controls

### Events

Exactly one event per received frame.

| Event | Raised when |
|---|---|
| `stp_bpdu_received` | a configuration or RST BPDU arrives with the topology-change flag **clear** |
| `stp_topology_change` | a TCN BPDU (type 0x80) arrives, **or** a config/RST BPDU carries the topology-change flag |

Routing a TCN to `stp_bpdu_received` would make an operator's topology-change
handler never match, so the split is asserted by
`a_topology_change_notification_raises_the_topology_change_event`.

Both carry the decoded BPDU as structured fields — `root_bridge_mac`,
`root_priority`, `root_system_id_extension`, `root_path_cost`, `bridge_*`,
`port_priority`, `port_number`, a `flags` object, the four timers **in seconds**,
`is_rstp`, `is_tcn`, `source_mac`, `destination_mac` — plus a `local_*` block
holding this server's own configured identity, so the model can compare what
arrived against what it is configured to claim. That comparison *is* the root
election question. `stp_topology_change` adds `change_reason`
(`tcn_bpdu` / `topology_change_flag`).

No hex string, no byte blob, in either direction.

### Actions

| Action | Effect |
|---|---|
| `send_stp_bpdu` | a configuration BPDU (`protocol_version: "stp"`) or RST BPDU (`"rstp"`). Every field optional; omissions come from the startup parameters. |
| `send_stp_tcn` | a Topology Change Notification. Carries no fields — its content *is* "something changed". |
| `no_bpdu` | explicit silence. A real answer, and the right one whenever unsure. |

`execute_action` builds and encodes the BPDU to validate it, then returns
`ActionResult::Custom { name: "stp_action", .. }`; the transport layers the
operator's configured defaults on top and encodes again for the wire. Validating
at execution time is what makes the declared `example` executable on its own
(`tests/executable_examples_test.rs` calls `execute_action` with nothing else in
scope) and what puts a bad priority in front of the model rather than in a log.

The default `port_role` for an RST BPDU is `designated` — a bridge that transmits
a BPDU on a segment is by definition claiming to be the designated bridge for it.

## Startup parameters

All ten are read by `StpBridgeConfig::from_startup_params` and used twice: they
fill in whatever the model's action omitted (`apply_defaults`), and they appear on
every event as `local_*`. Nothing is declared and unread.

| Parameter | Default | Notes |
|---|---|---|
| `transport` | `raw` | `raw` or `udp` |
| `bridge_mac` | `02:00:00:00:00:01` | locally administered, cannot collide with a vendor OUI |
| `bridge_priority` | 32768 | 0..=61440, steps of 4096. **Lower wins the root election.** |
| `system_id_extension` | 0 | 0..=4095, the VLAN id |
| `port_priority` | 128 | 0..=240, steps of 16 |
| `port_number` | 1 | 0..=4095 |
| `protocol_version` | `rstp` | `stp` (802.1D, v0) or `rstp` (802.1w, v2) |
| `hello_time` | 2 | whole seconds; encoded as 1/256 s |
| `max_age` | 20 | whole seconds |
| `forward_delay` | 15 | whole seconds |

An unencodable identity refuses at **startup** rather than failing on every BPDU
it later tries to send: `bridge_priority: 32769` returns `Err` from `spawn()`
naming the 4096 step.

Received BPDUs of either version are always decoded; `protocol_version` only
decides what this bridge emits.

## The privilege gate is per-protocol, not per-transport

Worth knowing before you try to start this through the TUI, MCP or the e2e
harness: `server_startup`'s check is
`requires_privileges = !privilege_met` for `RawSockets`, evaluated **before** the
startup parameters are read. So an unprivileged `start_server` is refused even
with `transport: "udp"`, which needs no privilege at all.

That is not a defect in the gate — declaring anything weaker would be a lie about
the raw transport, which is the real one. It does mean the UDP transport is
reachable only by calling `Server::spawn(ctx)` directly, which is what
`tests/server/stp/e2e_test.rs` does. If you want the UDP transport usable from
the dashboard, the fix belongs in `server_startup` (a per-transport privilege
query), not here, and it touches a shared file.

## Not implemented

No spanning tree state machine, no port state (blocking/listening/learning/
forwarding), no root election, no timers, no MAC learning, no topology change
propagation, no BPDU guard/root guard, no per-VLAN instance handling beyond
carrying the system ID extension, no MSTP (version 3) encoding — a version-3 BPDU
is *decoded* with its version reported verbatim, but its MSTP extension after
offset 35 is not parsed. No storage of any kind.

`pnet` is a declared dependency of the feature and is currently unused: the codec
is hand-written, and `pcap` alone covers capture and injection. Left declared
because it is the natural home for any future MAC/interface helper and the
feature line is a shared file.

## Proven and unproven — read this before rating it

**Proven:**

* The codec, against literal specification bytes in **both** directions —
  configuration BPDU, RST BPDU and TCN, including the 1/256-second timer encoding
  at its exact offsets and the 4/12-bit priority packing. See
  `tests/server/stp/CLAUDE.md` for the provenance of the literals and why they are
  not circular evidence.
* The whole decision path over the UDP transport, unprivileged: decode → event →
  handler/LLM dispatch → action → re-encode → transmit, with the response frame
  decoded and asserted field by field.
* Silence on LLM failure, asserted together with the log line that proves the
  path was reached.
* `spawn()` refusing rather than reporting a phantom `Running` when the capture
  handle cannot open.

**Unproven:**

* **The raw 802.3 transport has never been executed.** No test here runs
  privileged. Every line of the pcap path — the open, the filter, the injection
  thread, the cooperative stop — is untested code written to the shape of `arp`
  and `isis`.
* **No third-party STP peer has ever spoken to this server.** `mstpd`, a real
  switch, or another bridge: none of them, ever.

`Experimental` is therefore the honest rating and it is not close. Beta means
"works against real clients", and nothing here has met a real client. Do not
promote it on the codec tests alone — that is the mistake the root `CLAUDE.md`
records for `wireguard`, which held `Stable` on a test that mocked events the
implementation did not have.

## The concrete path to Beta

This machine has the `feth` driver (`net.link.fake.txstart: 1`), which gives a
real Ethernet pair with no hardware:

```bash
sudo ifconfig feth0 create
sudo ifconfig feth1 create
sudo ifconfig feth1 peer feth0
sudo ifconfig feth0 up && sudo ifconfig feth1 up
```

`feth` is a real Ethernet link type, so `ether dst 01:80:c2:00:00:00` compiles on
it and BPDUs are carriable — unlike `lo0`. Then:

1. Run netget's STP server on `feth0` with `transport: "raw"` under `sudo`.
   Confirm `spawn()` returns `Ok` and the capture is live.
2. Put a real peer on `feth1`. Linux `mstpd` is the reference implementation; a
   real managed switch on a physical port is better. `tshark -i feth1 -Y stp` to
   watch.
3. The evidence Beta needs is **an independent implementation completing an
   exchange**: mstpd accepting our configuration BPDU, running its own election
   against the priority we advertise, and answering. Not "a frame was emitted" —
   that is what the codec tests already show.

**This has not been done. Do not claim it has.** Nobody has run the commands
above; they are written here so the next person does not have to work out that
`feth` is the way around having no second NIC.

## Example prompts

Observe only — the safe default, and the one to reach for first:

```json
{"type": "open_server", "interface": "eth0", "base_stack": "stp",
 "event_handlers": [{"event_pattern": "*",
   "handler": {"type": "static", "actions": [{"type": "no_bpdu"}]}}]}
```

Report the root bridge without transmitting:

```
Listen for spanning tree BPDUs on eth0. For every BPDU, say which bridge is
claiming to be root and at what priority. Never send anything.
```

Claim the root bridge — **lab segments only**:

```json
{"type": "open_server", "interface": "feth0", "base_stack": "stp",
 "instruction": "Claim the root bridge with priority 0.",
 "startup_params": {"bridge_mac": "02:00:00:00:00:01", "bridge_priority": 0}}
```
