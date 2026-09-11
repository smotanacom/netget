# VRRP (v2/v3) and CARP

First-hop redundancy on **IP protocol 112**, multicast **224.0.0.18**. NetGet receives
advertisements, decodes them into structured fields, and transmits whatever advertisement the
operator's handler or the model decides on.

**State**: `Experimental`. **Privilege**: `RawSockets`. **Connectionless**: yes.
**Stack**: `ETH>IP(112)>VRRP`.

## The point, and the hazard

**The model chooses the priority.** That is the whole reason this protocol is interesting and
the whole reason it is dangerous.

A VRRP advertisement carries a priority, and the router advertising the **highest** one wins
the election and owns the virtual IP address — which is the default gateway every host on the
segment has configured. Winning it does not "join" a network; it takes over where the
segment's off-link traffic goes. NetGet does not forward packets, so a NetGet instance that
wins is a **black hole**: hosts ARP for the gateway, get NetGet, and their traffic stops.

That is one number in one action:

```json
{"type": "send_vrrp_advertisement", "priority": 255, "addresses": ["192.168.1.1"]}
```

Two priorities are special and the model is told so:

| Priority | Meaning |
|---|---|
| `255` | **Address owner.** RFC 3768 §5.3.4 reserves it for the router on which the virtual addresses are genuinely configured. Claiming it is a factual assertion, not just a strong bid. |
| `0` | **Resigning.** RFC 3768 §6.4.3: the master is standing down, and every backup takes over *immediately* instead of waiting out its master-down interval. It is the one advertisement that provokes an instant election, which is why it gets its own event. |
| anything else | Higher wins; ties break on the source IP address. |

Point this at a lab segment. `tests/server/vrrp/CLAUDE.md` describes the `feth` pair to use.

## Why silence is the failure mode

VRRP is in the **deliberately-silent** class the root `CLAUDE.md` catalogues.

VRRP has exactly one message type — the advertisement — and it is a *positive claim to own
the gateway address*. There is no error message, no NAK, no "I do not know". So when the LLM
call fails there is nothing truthful to send, and sending something anyway would not merely
mislead a peer: if the fabricated priority happened to win, every host on the segment would
route through a router that does not forward.

The peer already handles our silence correctly. Its own master-down interval expires and it
takes over — that is the spec-defined outcome for "that router is gone", and it is exactly
right.

So the LLM-failure path in `dispatch_event` writes **nothing** to the wire and records the
failure in the log, the way `src/server/radius/` keeps its cases apart. Nothing derived from
the error reaches a packet; the error is classified with `crate::utils::WireFailure` only to
keep an overloaded backend distinguishable from a broken one in the operator's log.

| `decision=` | Meaning |
|---|---|
| `no_policy` | No instruction and no handler configured. Observed passively, **no LLM call at all**. |
| `model_reject` | The model chose `no_advertisement`. A real answer: observe, transmit nothing. |
| `model_silent` | The model returned no usable action. Not a decision. |
| `model_invalid` | The action could not be encoded (a VRRPv3 interval over 40.95 s, a `priority` on a CARP server). Nothing transmitted. |
| `fail_closed_overloaded` | The LLM backend was saturated. Nothing transmitted. |
| `fail_closed_llm_error` | The LLM call failed some other way. Nothing transmitted. |

All six look identical on the wire. The log is the only place the difference survives, which
is why the tags are stable and greppable.
`tests/server/vrrp/e2e_test.rs::an_llm_failure_puts_nothing_on_the_wire` asserts the pair that
makes this meaningful: the `decision=fail_closed_` line proves the packet was decoded, the
event raised and the model asked, and the absent packet proves the failure produced no output.

## There is deliberately no election state machine

Nothing here runs a master-down timer, tracks a state, or transmits on its own. NetGet supplies
the wire; the model supplies the decisions, one advertisement at a time.

This is not an omission to be filled in later. An implementation that elected itself master and
then kept advertising on a timer would go on asserting gateway ownership after the model
stopped answering — an automatic master NetGet cannot back, which is exactly the fail-open
shape the root `CLAUDE.md` forbids. The silence guarantee above only means anything because
there is no background transmitter to undermine it.

It is also what keeps the no-storage rule: the server holds no neighbour table, no election
state, no learned anything.

## Architecture: the codec is separate from the transport, and that is the point

You cannot open a raw socket in this environment — nothing here runs as root. So the protocol
is split so that the part which *can* be proved is proved:

```
codec.rs     pure, no I/O, no async, no AppState.  Bytes <-> structured values.
             Proved against literal specification bytes, both directions.
mod.rs       two thin transports over it.
             Raw (SOCK_RAW, IP proto 112): NEVER EXECUTED.
             UDP:                          exercised end to end, unprivileged.
actions.rs   what the model sees; startup parameters; action -> codec values.
```

This is the `bluetooth_ble_beacon` precedent: pure construction exhaustively tested against
literal spec bytes, transport never executed, and `metadata().notes` saying **both**.

### Three formats, one protocol number — and the first octet does not tell them apart

| | VRRPv2 (RFC 3768) | VRRPv3 (RFC 5798) | CARP (OpenBSD) |
|---|---|---|---|
| octet 0 | `0x21` | `0x31` | **`0x21`** |
| interval unit | whole seconds, 1 octet | **centiseconds**, 12 bits | `advbase` seconds + `advskew`/256 |
| octet 4 | Auth Type | 4 reserved bits + interval high nibble | `carp_demote` |
| checksum | over the message | over an **IP pseudo-header** + the message | over the message |
| trailer | 8 zeroed auth-data octets | none | 8-octet counter + 20-octet HMAC-SHA1 |
| length | 8 + 4·n + 8 | 8 + 4·n | always 36 |
| election | priority, **higher wins** | same | advskew, **lower wins** |

VRRPv2's version/type octet is `0x21`. CARP's is *also* `0x21` (`CARP_VERSION` 2,
`CARP_ADVERTISEMENT` 1). Worse, CARP's `carp_authlen` (7) sits exactly where VRRP's
count-IP-addresses octet does, and 8 + 7·4 is exactly 36 — so a VRRP decoder accepts a CARP
packet *without erroring*, as an advertisement claiming seven virtual addresses. And CARP's
`carp_advskew` sits where VRRP's priority does, so a perfectly healthy CARP host reads as a
**VRRP master resigning**, the one advertisement that provokes an immediate election.

`codec_test.rs::a_carp_advertisement_misdecodes_as_a_vrrpv2_resignation` demonstrates exactly
that. It is why the packet format comes from the `variant` startup parameter and is never
sniffed.

### The two things implementations get wrong, both pinned to literal bytes

1. **Seconds versus centiseconds.** A one-second interval is `0x01` in VRRPv2 and `0x0064`
   (100) in VRRPv3. Writing seconds straight into the v3 field yields an advertisement
   claiming a 10-millisecond interval, and a real peer sizes its master-down interval from it.
   The codec exposes seconds and does the scaling in one place (`interval_field`); the test
   asserts the encoded bytes at their offsets *and* asserts they differ from the naive
   encoding, so a regression says what went wrong rather than reporting `1 != 100`.

2. **The VRRPv3 checksum is not the VRRPv2 checksum.** RFC 3768 §5.3.8 sums the VRRP message
   alone. RFC 5798 §5.2.8 sums an IP pseudo-header — source, destination, zero, protocol 112,
   length — *and* the message. So the same body addressed to `224.0.0.18` and to a unicast
   peer carries different octets. `VrrpAdvertisement::encode` **refuses** a v3 encode with no
   `PseudoHeader` rather than producing a message-only checksum: a silently-wrong checksum is
   discarded by every conformant peer, which looks exactly like the server being down.

### CARP's HMAC, and what is and is not claimed about it

There is no CARP RFC. The layout and the HMAC construction are read out of OpenBSD's
`sys/netinet/ip_carp.{h,c}`:

* the key is the passphrase **zero-padded or truncated to 20 octets** — `ifconfig carp pass`
  copies the raw bytes into `carpr_key` without hashing them;
* the message is `version || type || vhid || each virtual address || counter`, with version
  and type as two separate octets (`0x02`, `0x01`), not the packed `0x21` of the header, and
  the counter in the big-endian form it takes on the wire.

**The hash is the `sha1` crate's** — the `vrrp` feature declares `dep:sha1`, and `codec::sha1`
is a thin adapter that returns a `[u8; 20]` so the rest of the file need not thread
`GenericArray` around. Only the RFC 2104 HMAC construction on top is written here, because no
HMAC crate is reachable from this feature (`hmac` is optional and gated behind `tor`).

This file used to carry a hand-written SHA-1, pinned to the published vectors. Vectors prove
the happy path, not the edge cases, and the next person to touch a hand-rolled compression
function will not have the context of whoever wrote it — so it is gone. **The vector tests are
not**: they were kept with their job changed, and that is the part worth understanding.
`tests/server/vrrp/codec_test.rs` still runs FIPS 180 / RFC 3174 and RFC 2202 through this
code, and still sweeps every input length 0..=130, but they now assert that *we drive the
crate correctly* rather than that SHA-1 is correct. An adapter that hashed the wrong buffer,
dropped an `update`, truncated the digest or swapped the `0x36`/`0x5c` pads would pass every
other test in that file and fail these. The sweep's oracle moved from "the `sha1` crate"
— tautological now that we call it — to "the `sha1` crate's *streaming* API", fed one octet at
a time, which is a genuinely different path through it.

**What is proved is the primitive and how we drive it, not the CARP-specific input ordering.**
Which fields, in which order, with which key derivation, is still a reading of `ip_carp.c` that
no OpenBSD `carp` interface has ever accepted. If a `carpd` rejects our advertisements, the
input construction is the first thing to doubt — the hash underneath is not the suspect it was
when this was hand-rolled.

With **no** `carp_passphrase` configured, an inbound HMAC is reported to the model as
`hmac_valid: null`, never `true`. Reporting an unchecked signature as valid is the fail-open
shape the OAuth2 post-mortem in the root `CLAUDE.md` describes.

### The two transports

**Raw (`transport: "raw"`, the default).** `SOCK_RAW` on IP protocol 112, joined to
`224.0.0.18` on the interface address, multicast TTL 255 (RFC 5798 §5.1.1.3). The IPv4 header
is stripped on receive and its source/destination become the VRRPv3 pseudo-header, which is
the only place a checksum can be checked properly. Creating the socket is synchronous and is
the privileged step, so its failure reaches the caller directly and **nothing is spawned
before it succeeds** — the ARP/DataLink/ICMP fire-and-forget defect the root `CLAUDE.md`
records is designed out rather than left to be found later.

**UDP (`transport: "udp"`).** One **complete VRRP or CARP message per datagram** — the same
octets the raw transport would put on the wire, with only the IP layer simulated. Replies go
back to the datagram's sender. This exists so the full advertisement → event → model → action
→ packet path runs unprivileged, which is the compromise `ospf` documents as "requires
root/CAP_NET_RAW, tests use UDP", done inside the protocol instead of by substituting a
generic UDP server in the test.

Because VRRPv3 needs IP addresses it does not have there, the UDP transport reconstructs the
pseudo-header by convention: **inbound**, the datagram's sender and `224.0.0.18`; **outbound**,
the bound address and whatever `destination` the action resolved to. VRRPv2 and CARP need none
of this, and the test literals are v2/CARP precisely so nothing in the suite depends on the
convention.

## The privilege gate is per-protocol, not per-transport

Worth knowing before you try to start this through the TUI, MCP or the e2e harness:
`server_startup`'s check is `requires_privileges = !privilege_met` for `RawSockets`, evaluated
**before** the startup parameters are read. So an unprivileged `start_server` is refused even
with `transport: "udp"`, which needs no privilege at all.

That is not a defect in the gate — declaring anything weaker would be a lie about the raw
transport, which is the real one. It does mean the UDP transport is reachable only by calling
`Server::spawn(ctx)` directly, which is what `tests/server/vrrp/e2e_test.rs` does. If you want
the UDP transport usable from the dashboard, the fix belongs in `server_startup` (a
per-transport privilege query), not here, and it touches a shared file. `stp` has the same
constraint for the same reason.

## What the model sees and controls

### Events

| Event | Raised when |
|---|---|
| `vrrp_advertisement_received` | any advertisement whose priority is **not** 0, and every CARP advertisement |
| `vrrp_master_resigned` | a VRRP advertisement with priority 0 |

The split exists because they are different questions: one is "somebody is master", the other
is "the master just stood down and the election is open **right now**". Routing a resignation
to the wrong event would make an operator's handler for it never match, and a resignation is
precisely the moment a takeover succeeds.

Both carry the decoded packet as structured fields, plus a `local_*` block holding this
server's own configured group — so the model can compare what arrived against what it is
configured to claim. That comparison *is* the election question.

**The two variants do not carry the same keys, and a handler must not assume they do.** They
are different protocols; only the fields both packets actually have are common.

| | VRRP | CARP |
|---|---|---|
| common | `variant`, `version`, `vrid`, `advert_interval` (seconds, already converted from whichever unit the version uses), `source_address`, `checksum_valid`, `checksum_scope` | same |
| VRRP only | `priority`, `addresses` (dotted quads), `address_count`, `auth_type`, `is_address_owner`, `is_resignation` | — |
| CARP only | — | `advskew`, `advbase`, `demote`, `counter`, `hmac_valid` |

The asymmetry is the protocols', not an omission: CARP has no priority and no virtual-address
list on the wire, and VRRP has no skew, no demotion counter and no HMAC.

No hex string, no byte blob, in either direction. The CARP HMAC is never handed to the model:
it gets `hmac_valid`, which is a decision, not 20 opaque octets.

### Actions

| Action | Effect |
|---|---|
| `send_vrrp_advertisement` | one advertisement, VRRP or CARP. Every field optional; omissions come from the startup parameters. |
| `no_advertisement` | explicit silence. A real answer, and the right one whenever unsure. |

`execute_action` builds and validates the advertisement — without a checksum, which is the
only step needing addresses — then returns `ActionResult::Custom { name: "vrrp_action", .. }`;
the transport layers the operator's configured group on top and encodes for the wire.
Validating at execution time is what makes the declared `example` executable on its own
(`tests/executable_examples_test.rs` calls `execute_action` with nothing else in scope) and
what puts a bad interval in front of the model rather than in a log.

**Fields belonging to the other variant are refused, not dropped.** `priority` on a CARP
advertisement is an error naming `advskew` (and saying that lower wins there); `advskew` on a
VRRP one is an error naming `priority`. Silently ignoring a field the model deliberately set
is how a model's decision becomes invisible.

Because `execute_action` has no server configuration in scope, it **infers** the variant when
the action does not name one: an explicit `variant` wins, otherwise a CARP-only field with no
VRRP-only field implies CARP. A model talking to a CARP server has no reason to spell the
variant out, and rejecting its `advskew` there would make the whole CARP path unreachable —
which is what happened the first time this was written, and what
`execute_action_infers_the_variant_when_the_configuration_is_not_in_scope` guards. On the wire
the configured variant always decides, because `apply_defaults` writes it into the action
before the transport re-validates.

## Startup parameters

All nine are read by `VrrpGroupConfig::from_startup_params` and used twice: they fill in
whatever the model's action omitted (`apply_defaults`), and they appear on every event as
`local_*`. Nothing is declared and unread.

| Parameter | Default | Notes |
|---|---|---|
| `transport` | `raw` | `raw` or `udp` |
| `variant` | `vrrp` | `vrrp` or `carp`. Decides how received packets are decoded — see the 0x21 collision above. |
| `version` | `3` | 2 (RFC 3768) or 3 (RFC 5798). Ignored for CARP. |
| `vrid` | 1 | 1..=255. CARP calls it the vhid. |
| `priority` | 100 | 0..=255. **Higher wins the election.** Not 255 by default: claiming address ownership by default would assert something untrue. |
| `advert_interval` | 1 | Whole seconds, 1..=255 |
| `addresses` | `[]` | Dotted quads. Empty by default — advertising an address by default would claim one nobody asked for. |
| `advskew` | 0 | CARP only, 0..=255. **Lower wins.** |
| `carp_passphrase` | *(none)* | CARP only. Keys the HMAC. Never logged or shown to the model; only `local_carp_passphrase_configured` is. |

`advert_interval` is whole seconds here because `StartupParams` has no fractional accessor and
adding one means editing a shared file. VRRPv3's sub-second range (0.01..=40.95 s) is reachable
through the action's `advert_interval` field, which is raw JSON.

**A group whose identity cannot be encoded refuses at `spawn()`**, not on every advertisement
it later tries to send. The interesting case is cross-field: `advert_interval: 60` is perfectly
legal under VRRPv2 and impossible under VRRPv3, whose 12-bit centisecond field tops out at
40.95 s. A per-parameter range check cannot catch that, which is why
`VrrpGroupConfig::from_startup_params` ends by building a template advertisement and validating
it.

## Not implemented

No election state machine, no master-down timer, no periodic transmitter, no preemption logic,
no virtual-MAC (`00:00:5E:00:01:{VRID}`) handling, no gratuitous ARP on takeover, no IPv6
(`FF02::12`) in either direction, no VRRPv2 RFC 2338-era authentication (the field is decoded
and surfaced, never verified), no CARP counter/replay tracking. No storage of any kind.

The feature is `vrrp = ["socket2/all", "dep:sha1"]`. It briefly carried `pnet`, which nothing
here ever used — the codec is hand-written and `socket2`/`libc` cover the raw socket — and it
was dropped once both this protocol and `stp` reported it unused.

## Proven and unproven — read this before rating it

**Proven:**

* The codec, against literal specification bytes in **both** directions — VRRPv2, VRRPv3 and
  CARP, including the seconds-versus-centiseconds interval at its exact offsets, both checksum
  scopes with the sums worked through by hand, and CARP's 36-octet layout. See
  `tests/server/vrrp/CLAUDE.md` for the provenance of the literals and why they are not
  circular evidence.
* That this code **drives** SHA-1 and HMAC-SHA1 correctly — the hash itself is the `sha1`
  crate's — against FIPS 180 / RFC 3174 and RFC 2202 vectors, plus a 0..=130 length sweep
  against the crate's streaming API. The RFC 2104 construction on top is ours and the RFC 2202
  vectors are its oracle.
* The whole decision path over the UDP transport, unprivileged: decode → event → handler/LLM
  dispatch → action → re-encode → transmit, with the response packet decoded and asserted
  field by field, in both VRRP and CARP.
* Silence on LLM failure, asserted together with the log line that proves the path was reached.
* `spawn()` refusing rather than reporting a phantom `Running` when the raw socket cannot open.

**Unproven:**

* **The raw IP-protocol-112 transport has never been executed.** No test here runs privileged.
  The socket creation, the multicast join, the IPv4-header strip and the `sendto` are untested
  code written to the shape of `ospf`.
* **No third-party VRRP or CARP peer has ever spoken to this server.** Not `keepalived`, not
  `frr`, not an OpenBSD `carp` interface, not a real router.
* **CARP's HMAC input construction** is a reading of OpenBSD source, not of a specification,
  and nothing has ever accepted it.

`Experimental` is therefore the honest rating and it is not close. Beta means "works against
real clients", and nothing here has met one. Do not promote it on the codec tests alone — that
is the mistake the root `CLAUDE.md` records for `wireguard`, which held `Stable` on a test that
mocked events the implementation did not have.

## The concrete path to Beta

This machine has the `feth` driver (`net.link.fake.txstart: 1`), which gives a real Ethernet
pair with no hardware:

```bash
sudo ifconfig feth0 create
sudo ifconfig feth1 create
sudo ifconfig feth1 peer feth0
sudo ifconfig feth0 inet 192.0.2.1/24 up
sudo ifconfig feth1 inet 192.0.2.2/24 up
```

Then:

1. Run netget's VRRP server on `192.0.2.1` with `transport: "raw"` under `sudo`. Confirm
   `spawn()` returns `Ok` and the multicast join succeeded.
2. Put `keepalived` on the other end (`vrrp_instance` on `feth1`, same `virtual_router_id`).
   `tshark -i feth1 -Y vrrp` to watch. `keepalived` is the reference implementation and it is
   brewable; for CARP the peer would have to be an OpenBSD host with a `carp` interface, which
   this machine cannot provide.
3. The evidence Beta needs is **an independent implementation completing an exchange**:
   keepalived accepting our advertisement, running its own election against the priority we
   advertise, and transitioning MASTER/BACKUP accordingly. Not "a packet was emitted" — that is
   what the codec tests already show.

**This has not been done. Do not claim it has.** Nobody has run the commands above; they are
written here so the next person does not have to work out that `feth` is the way around having
no second NIC.

## Example prompts

Observe only — the safe default, and the one to reach for first:

```json
{"type": "open_server", "host": "192.0.2.1", "base_stack": "vrrp",
 "event_handlers": [{"event_pattern": "*",
   "handler": {"type": "static", "actions": [{"type": "no_advertisement"}]}}]}
```

Report who holds the gateway without contesting it:

```
Listen for VRRP advertisements on 192.0.2.1. For every one, say which router is master, at
what priority, and which virtual addresses it claims. Never advertise.
```

Take the gateway — **lab segments only**:

```json
{"type": "open_server", "host": "192.0.2.1", "base_stack": "vrrp",
 "instruction": "Claim VRID 1 with priority 255 whenever anyone else advertises.",
 "startup_params": {"vrid": 1, "priority": 255, "addresses": ["192.0.2.100"]}}
```
