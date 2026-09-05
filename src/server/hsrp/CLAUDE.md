# HSRP Protocol Implementation

Hot Standby Router Protocol: a first-hop redundancy protocol. A group of routers share one
**virtual IP**, elect one of themselves **Active**, and that one answers for the address every
host on the segment has configured as its default gateway.

**State**: `Experimental`. The transport genuinely executes in the test suite — see
[Maturity](#maturity-why-experimental) for what that does and does not buy.
**Privilege**: `PrivilegeRequirement::None`. **Port 1985 is above 1023**, and joining a
multicast group needs no elevation, so unlike every other protocol in its tier this one runs
unprivileged and is really exercised rather than mocked away.
**Stack**: `ETH>IP>UDP>HSRP`. **Connectionless**: yes, declared.

---

## Read this first: the model can take over the segment's default gateway

**The model chooses the priority and may send a Coup. If it wins, NetGet becomes the active
gateway for every host on the link — and NetGet does not forward traffic.** A won election is
therefore not a cosmetic outcome: it is a **black-holed segment**. Everything below follows
from that.

Three design consequences, none of them optional:

1. **An LLM failure produces SILENCE.** Nothing is written, ever, on any failure path.
2. **There is no autonomous election state machine.** This server advertises only in reply to a
   datagram that actually arrived. It runs no timers and never speaks unprompted.
3. **`get_async_actions()` is empty, deliberately.** An async action is one the model can fire
   with no network event — i.e. a way to start advertising on its own initiative. That is
   exactly the autonomous Active router point 2 forbids, so the vocabulary does not exist.

There is deliberately **no guard that refuses a Coup**. The model is in charge, as everywhere
else in NetGet; what the server owes the operator is that the claim is *loud*. Every
advertisement whose opcode is Coup or whose state is Active logs a `WARN` on both channels
naming the virtual IP and the priority, and `tests/server/hsrp/e2e_test.rs` asserts that line
appears.

---

## Two wire formats that share only a name

**HSRPv1 and HSRPv2 are not one protocol with a version field.** They are unrelated encodings.
Nothing about one parses as the other, they use different multicast groups, and — the trap
worth remembering — **their state numbers overlap with different meanings**.

`codec.rs` is the only place either layout exists. It is pure: no sockets, no LLM, no state.

### HSRPv1 — RFC 2281 §5, a flat 20 bytes

```text
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|   Version     |   Op Code     |     State     |   Hellotime   |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|   Holdtime    |   Priority    |     Group     |   Reserved    |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                   Authentication  Data (8 bytes)              |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                    Virtual IP Address                         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

**The Version byte is `0`, not `1`** (RFC 2281 §5: *"Currently, this is version 0"*). Writing 1
there produces a packet no real speaker accepts, and it is the first thing to check if interop
ever fails. `codec::V1_VERSION_BYTE` exists so nobody has to remember.

Group 224.0.0.2:1985 (the all-routers group). Times are **seconds**, one byte each. Priority
and group are **one byte each**.

### HSRPv2 — Cisco's TLV format

```text
Type=1 Len=40 | Version=2 | Opcode | State | IP Ver | Group(2)
Identifier(6) | Priority(4) | Hellotime(4, ms) | Holdtime(4, ms) | Virtual IP(16)
[ Type=3 Len=8  | Authentication text (8) ]
[ Type=4 Len=28 | Algorithm | Pad | Flags(2) | Sender IP(4) | Key ID(4) | Digest(16) ]
```

Group 224.0.0.102:1985 for IPv4 — **not** v1's 224.0.0.2; v2 moved off the all-routers group
precisely so v1 speakers do not see its TLVs. FF02::66:2029 for IPv6.

Times are **milliseconds**. Priority is **32 bits**, group is **12 bits** (0–4095). The 6-byte
Identifier (in practice the sender's MAC) has no v1 equivalent. The virtual IP field is 16
bytes whatever the family; an IPv4 address occupies the first four and the rest stay zero.

### The state-code collision

| state | HSRPv1 code | HSRPv2 code |
|---|---|---|
| Initial | 0 | 0 |
| Learn | 1 | 1 |
| Listen | 2 | 2 |
| **Speak** | **4** | 3 |
| **Standby** | 8 | **4** |
| Active | 16 | 5 |

v1 is a bitmask; v2 renumbered densely. **Code `4` means Speak in v1 and Standby in v2.** This
mis-decodes silently — both are valid states, so nothing errors — which is why:

* `HsrpState` is never converted with a bare `as u8`. Everything goes through
  `v1_code`/`v2_code` and `from_v1_code`/`from_v2_code`.
* The model sees and produces **state names**, never numbers. `from_str_name` accepts no
  integers at all, deliberately: allowing them would reintroduce the collision at the one
  boundary most likely to get it wrong.
* The e2e suite pins it **from both sides** — a v1 server must report code 4 as `speak`, and a
  v2 server must report the same code as `standby`.

### Telling them apart on receive

`codec::decode` sniffs the first byte rather than trusting the configured version: v1's Version
byte is `0` and a v2 TLV type never is. That is deliberate — a real segment can carry both at
once, and refusing to parse the version the operator did not configure would simply hide a
neighbour from the model.

---

## Authentication: there isn't any

The v1 authentication field, and the v2 Text Authentication TLV, are **eight bytes of NUL-padded
plaintext, sent in the clear in every packet**. The near-universal default is the ASCII string
`"cisco"` (`63 69 73 63 6f 00 00 00`), which is implemented here and is what `send_hsrp_hello`
defaults to for v1.

**This provides no security whatsoever.** Anyone on the segment reads it out of the first packet
they see. It is a misconfiguration guard — it stops a router in the wrong group from joining
your election by accident — and nothing more. The action description says so to the model.

**HSRPv2 MD5 authentication is parsed but neither verified nor generated.**

* *Parsed*: an inbound MD5 TLV is reported to the model structurally — `algorithm`, `flags`,
  `sender_address`, `key_id`. The 16-byte digest is deliberately **not** in the event: it is raw
  bytes, which the root `CLAUDE.md` forbids in event data, and it is unverifiable anyway.
* *Not verified*: verification needs the shared key. NetGet holds no key material and protocols
  here implement no storage.
* *Not generated*: `execute_action` **refuses** an action carrying `md5_key`/`md5_key_id`, with
  the reason named. An invalid digest would be worse than no TLV — a peer configured for MD5
  logs it as an attack rather than as a misconfiguration.

---

## What the model sees and controls

### Events

Three, one per opcode, each with a real emit site in `mod.rs` selected by
`actions::event_for_opcode`:

| event | raised by |
|---|---|
| `hsrp_hello_received` | a neighbour announcing itself |
| `hsrp_coup_received` | a neighbour seizing the Active role |
| `hsrp_resign_received` | a neighbour giving the Active role up |

All three carry the same fields, because the packet is the same for all three opcodes:
`version`, `opcode`, `state`, `priority`, `group`, `hellotime`, `holdtime`, `virtual_ip`,
`auth_data`, `identifier`, `md5_auth`, `source_address`, `configured_version`.

Structured only — no raw bytes, no base64. States and opcodes are names; addresses are dotted
quads; `auth_data` is the string with trailing NULs stripped (`None` if the field was empty).

### Actions

| action | effect |
|---|---|
| `send_hsrp_hello` | "I am here, in this state, at this priority." An assertion, not a query |
| `send_hsrp_coup` | Seize Active. **The most consequential action in this protocol** |
| `send_hsrp_resign` | Give Active up so the standby takes over without waiting out its timer |
| `no_advertisement` | **Say nothing.** A deliberate answer, logged as one, and always safe |

All four are sync actions attached to all three events via `.with_actions(...)` — without that
`call_llm` builds the model an empty tool list and it cannot answer at all.

Shared parameters (`advertisement_parameters()`, so the three cannot drift): `version`, `state`,
`priority`, `group`, `virtual_ip` required; `hellotime` (default 3), `holdtime` (default 10),
`auth_data` (default `"cisco"` for v1, absent for v2), `identifier` (v2 only) optional.

**`version` is required, not defaulted.** Defaulting it would let a model that forgot the field
put a v1 packet on a v2 segment, where it is silently ignored — which looks exactly like this
server having said nothing. Requiring it fails closed instead.

Times are expressed in **seconds** in both directions and converted to milliseconds for v2 at
the codec edge, so nothing above `codec.rs` has to remember which version uses which unit. The
cost: **HSRPv2's sub-second timers are not expressible.** That is a real limitation, not an
oversight.

---

## Fail-closed: an LLM failure produces silence

HSRP is in the **deliberately-silent** class the root `CLAUDE.md` catalogues, and its case is
one of the strongest in it:

* **The protocol has no negative message at all.** There is no error frame, no NAK, no refusal.
  The only thing this server *could* emit is an advertisement.
* **Every advertisement is a positive claim about who owns the gateway address.** A fabricated
  Hello during a backend outage can win an election NetGet cannot serve; a fabricated Coup takes
  the segment from a router that was working. Both black-hole the link.
* **Silence is what the protocol expects.** A peer that hears nothing keeps its own view of the
  election. That is the correct outcome when NetGet has nothing authorised to say.

So: **nothing is written, ever, on any failure path.** No `WireFailure` string ever reaches the
wire — there is no wire text here at all, HSRP being fixed binary — and the error is classified
(`overloaded` / `unavailable`) for the **log** only.

### The log carries the distinction instead

Copied from `src/server/radius/`, for the reason that file gives: *the model declined* and *the
model was never reached* produce identical bytes here — none — and must never be identical in
the log. Here the collapse would be especially invisible, because **a silent HSRP speaker is
entirely normal**; a total backend outage would look like protocol-correct quiet indefinitely.

`Decision` in `mod.rs`, logged as `decision=<token>` on every datagram:

| token | meaning | bytes sent |
|---|---|---|
| `model_response` | the model advertised | yes |
| `model_silent` | the model chose `no_advertisement` — it stayed out of the election | none |
| `fail_closed_no_action` | the model returned nothing usable | none |
| `fail_closed_action_error` | actions were produced, none encoded | none |
| `fail_closed_llm_error` | the LLM call failed — outage, overload, unusable output | none |

Every `fail_closed_*` is logged at ERROR on both channels; the rest at INFO.
`grep 'decision=fail_closed_'` finds every datagram the model did not actually answer.

**There is deliberately no `model_reject` token, unlike `radius`.** RADIUS can separate a
refusal from silence because it has an Access-Reject packet to send. HSRP has no negative
message of any kind: the only way a speaker declines to participate is by not advertising. So
"the model refused" and "the model chose to stay out" are the same act, and `model_silent` *is*
it. What must stay distinguishable — and does — is that act versus every `fail_closed_*`.

---

## Transport, and the two deviations from a real router

**UDP on the port** is the only path, and the multicast join is best-effort.

### Deviation 1: the multicast join is non-fatal

`join_hsrp_group` logs a failure on both channels and carries on. This is measured, not
assumed: on macOS a socket bound to `127.0.0.1` can **join** a group successfully and still
fail to **send** to one with `EADDRNOTAVAIL (49)`, because loopback carries no multicast route
(`PROTOCOL_ROADMAP.md` records the measurement). A speaker that refused to start over a join
would be unusable for exactly the local testing this repo does — while still being perfectly
able to handle a datagram sent straight to its port.

The group depends on the configured version, not just the address family: v1 → `224.0.0.2`,
v2 → `224.0.0.102`, IPv6 → `FF02::66`.

### Deviation 2: replies are unicast to the sender, not multicast to the group

A real HSRP router multicasts its Hellos to the group. This one sends its reply back to the
address the triggering datagram came from.

Two reasons, and the second is the load-bearing one. Sending to `224.0.0.2:1985` from a
loopback-bound socket fails with `EADDRNOTAVAIL`, so a group-addressed reply would make this
speaker unobservable in any local test. And more fundamentally: this server **never speaks
unprompted**, so it never has a reason to address the whole group. Every packet it sends is an
answer to a specific peer.

The consequence is real and worth knowing: on a genuine segment, other routers in the group
would **not** see NetGet's advertisements — only the one it replied to would. NetGet therefore
cannot actually participate in a multi-router election on real hardware as things stand.

---

## Startup parameters

| Parameter | Default | Effect |
|---|---|---|
| `version` | `1` | Which format this instance is *for*. Selects the multicast group joined, and is reported to the model as `configured_version`. Datagrams of the **other** version are still parsed and reported |
| `join_multicast` | `true` | Join the group. A failure is logged, never fatal |
| `multicast_interface` | host's choice | Local **IPv4** address to join on. No effect on an IPv6 socket, which selects by interface index |

All three are read in `mod.rs`, all three with `?` and never `unwrap()`.

---

## Not implemented

* **The election state machine.** No timers, no state tracking, no Active/Standby role held
  across datagrams, no preemption logic, no periodic Hellos. Deliberate — see the top of this
  file. NetGet supplies the wire; the model supplies the decisions.
* **Actually being a gateway.** NetGet does not forward traffic, answer ARP for the virtual IP,
  or install the virtual MAC (`00:00:0c:07:ac:XX` for v1). Winning an election here gets the
  segment a gateway that drops everything.
* **Multicast transmission.** See Deviation 2.
* **MD5 digest verification or generation.** See Authentication.
* **HSRPv2 millisecond timers.** Seconds only; see Actions.
* **The Interface State TLV** (v2 type 2) is skipped on parse and never generated.
* **Storage of any kind.** No group table, no neighbour cache. The model answers every datagram.

---

## Maturity: why Experimental

**The advantage this protocol has over the rest of its tier: no privilege is required.** Port
1985 is unprivileged, so the transport is not mocked or stubbed — `tests/server/hsrp/e2e_test.rs`
binds a real UDP socket, sends real datagrams into it, and asserts on the real bytes that come
back. That is more than `arp`, `isis`, `ospf` or `wireguard` can say, and it means the wiring is
genuinely proven: the event fires with the right fields, routing reaches it, the actions execute,
both packet layouts are byte-exact, and silence really is silent on two separate paths.

**What is missing is the only thing that separates it from Beta: no independent implementation
has ever accepted a packet from this server.** Checked, briefly:

| candidate | result |
|---|---|
| crates.io, HSRP | **nothing**. No HSRP crate exists in any role |
| `keepalived`, `vrrpd` | not installed — and they speak **VRRP**, a different protocol |
| a real Cisco device | the only genuine HSRP peer, and there is none here |

So the peer in the test suite is **hand-written from RFC 2281 and Cisco's HSRPv2
documentation**. The root `CLAUDE.md` is explicit that this is an independent *reading* of the
spec, not an independent implementation — the same standing as `dhcp`'s in-test RFC 2131 decoder
and `usb/serial`'s USB/IP client. It is the strongest evidence available, and it is not Beta
evidence.

**Specifically unverified:**

* Interoperability with anything. No Cisco device, or any other HSRP speaker, has seen these
  bytes.
* **The HSRPv2 layout in particular.** v2 has no RFC; the layout here comes from Cisco
  documentation and packet dissectors. It is more likely to be wrong than v1's.
* Multicast reception on a real link — the tests are unicast to loopback, and nothing here has
  ever processed a datagram that actually arrived via a group.
* IPv6 entirely. `FF02::66`, port 2029 and `join_multicast_v6` have never run.
* Participation in a real multi-router election, which Deviation 2 above currently precludes.

**Do not promote this on the strength of the test suite.** Promotion needs a Cisco device (or a
comparable real HSRP speaker) on a real segment, and that means a human with hardware.
