# NetGet Protocol Roadmap

Durable tracking for the protocol-expansion programme started September 2026:
what we decided to build, why, and where each item stands.

**This is not a status report.** The root `CLAUDE.md` warns against adding
one-off session/status files — that is what let the root directory reach 63
markdown files. This is one file, updated *in place* as items move, and it is
meant to be edited rather than superseded. If an item lands, change its row;
do not write a new file about it. When every row here reads `landed`, fold the
durable lessons into `CLAUDE.md` and delete this file.

## Where this stands (4 September 2026)

**All 30 protocols have landed**: 8 servers, 8 clients, 14 root/privileged. Four
are `Beta`; the other 26 are `Experimental`, each with its *reason* recorded
rather than a hedge.

Verified after the last one landed:

| Check | Result |
|---|---|
| `--features all-protocols` compile | clean (3 pre-existing lib warnings, none in the new 30) |
| Full suite, `--test-threads=100` | **1265 server + 360 client = 1625 passed, 0 failed** |
| Six whole-tree ratchets, at `all-protocols` | 29 passed, 0 failed |
| Orphaned test directories | none, either tree |
| New `#[cfg(test)]` under `src/` | none — the five pre-existing files are unchanged |
| `cargo fmt --check` | clean tree-wide |
| `/Users/matus/bin/netget` | rebuilt with `all-protocols` and reinstalled |

**Three test-deadline defects were found only by the full sweep**, all the same
shape: asserting a condition while waiting on something that happens *earlier*.
Two in `tuntap` (waiting on an arrival counter plus a fixed 120 ms settle, then
asserting a decision) and one in `gtp` (waiting for the model to be called, then
asserting on a log line written after its answer is handled). Each passes alone
in under a second at any feature set — which is exactly why the root `CLAUDE.md`
says passing in isolation tells you the *deadline* was wrong, not that the code
is fine. No assertion was changed; only what each waits for. GTP's wire
assertions had already passed, so its protocol behaviour was provably correct
and only the log line was in flight.

One failure seen mid-sweep was **`server::webrtc::…data_channel_message_round_trip`**,
which this programme never touched (`git log` confirms) and which passed on the
final run. It is load-flaky and pre-existing, and worth someone's attention:
under load its mock expectations go unmet and it trips the
`Server dropped without calling .verify_mocks()` guard.

## Status legend

| Status | Meaning |
|---|---|
| `planned` | Specified below. Nothing exists in the tree. |
| `wired` | Feature flag, module declaration, registry entry and test-module declaration exist. No implementation. |
| `building` | An agent is implementing it right now. |
| `landed` | Committed, compiles, its own tests pass. **Not** a claim about maturity — see below. |
| `blocked` | Needs something that does not exist yet. Reason recorded in the row. |

`landed` and the protocol's `DevelopmentState` are different claims. `landed`
means the code is in. `DevelopmentState` means what the evidence supports, and
the bar for `Beta` is a **real third-party client, not `#[ignore]`d, that
fails rather than skips when the client is missing**. Read the "Protocol
inventory" section of `CLAUDE.md` before promoting anything; three protocols
have been demoted for treating a codec or a skipped test as evidence.

## Environment constraints (measured on this machine, Darwin 27, September 2026)

These decide which items can be validated locally and which cannot. Re-measure
rather than trusting them.

| Fact | Consequence |
|---|---|
| **`feth` driver present** (`net.link.fake.txstart: 1`) | `sudo ifconfig feth0 create && ifconfig feth1 peer feth0` gives a real Ethernet pair with **no hardware**. Every Tier-1 L2 protocol below can be driven end to end locally. |
| **`/dev/bpf*` present** | Capture and injection available under root. |
| **`utun` available** (5 present) | TUN endpoint is viable on macOS. |
| **Multicast on loopback: joining works, *sending* does not** | Measured on macOS 27, and it contradicts the received wisdom that the *join* fails. Bound to `127.0.0.1`, `join_multicast_v4(239.255.255.250)` **succeeds**; `sendto(239.255.255.250:1900)` fails with **`EADDRNOTAVAIL` (49)** because loopback carries no multicast route. Bound to `0.0.0.0` both work. Any multicast protocol therefore needs a unicast target parameter to be observable in a test — do not "fix" a join that is not broken. |
| **No SCTP on macOS** (no headers, no stack) | M3UA/SIGTRAN cannot use its real transport here. See its row. |
| **No `AF_CAN` on macOS** | SocketCAN is Linux-only. Linux `vcan` needs no hardware, so it is testable *there*. |
| **802.11 injection unavailable on macOS** | Monitor mode works, injection effectively does not. Rogue-AP work would be Linux + specific chipset. Deferred. |
| Installed real clients | `curl` (with gopher), `finger`, `nmblookup`, `smbclient`, `whois`, `ffmpeg`, `docker`, `openssl`, `redis-cli`. No `nmbd`/`smbd` daemon, no `nats-server`, no UPnP tools. |

**The wireguard trap applies to everything privileged here.** A protocol that
cannot be started in the environment that tests it cannot be validated, and
`CLAUDE.md` records what happened when one was rated `Stable` anyway. Prefer
the items testable on `feth`/`utun` under sudo; for the Linux-only ones, either
run them in a Linux container in CI or rate them honestly and say why.

---

## Wave 1 — text and discovery servers

Eight servers. Wiring landed in `2f599954`; each implementation is committed
separately by the agent that wrote it.

| Protocol | Feature | Module | Transport | Privilege | Validation target | Status |
|---|---|---|---|---|---|---|
| NATS | `nats` | `nats` | TCP 4222 | none | **`async-nats` 0.50, official client, non-circular (we use no NATS library)** | `landed` (`b5d451ed`) — **Beta** |
| STOMP | `stomp` | `stomp` | TCP 61613 | none | **`async-stomp` 0.6.3** drives a full session and decodes every frame itself | `landed` (`f9b4e946`) — **Beta** |
| Ident | `ident` | `ident` | TCP 113 | `PrivilegedPort(113)` | **none exists** — no RFC 1413 client anywhere takes a configurable port | `landed` (`59a3003b`) — Experimental |
| Gopher | `gopher` | `gopher` | TCP 70 | `PrivilegedPort(70)` | **`curl gopher://` — real, arbitrary port** | `landed` (`a3573323`) — **Beta** |
| Finger | `finger` | `finger` | TCP 79 | `PrivilegedPort(79)` | `finger(1)` confirmed port-locked to 79 → needs root | `landed` (`a28bf0a2`) — Experimental |
| SSDP | `ssdp` | `ssdp` | UDP 1900 mcast | none for the *server*: no Rust SSDP client can be aimed at a unicast loopback port. **This does not generalise — see the correction below** | `landed` (`5cb49c9a`) — Experimental |
| LLMNR | `llmnr` | `llmnr` | UDP 5355 mcast | none | **none** — only LLMNR crate is a responder, not a querier; real clients are Windows/systemd-resolved. Evidence is circular by construction | `landed` (`19ebd0eb`) — Experimental |
| NetBIOS-NS | `netbios-ns` | `netbios_ns` | UDP 137 | `PrivilegedPort(137)` | `nmblookup` 4.24.6 **confirmed port-locked to 137** (`nbt port=` is parsed and ignored by the client; proven with tcpdump). Its captured datagrams pin the *decode* direction; the *response* direction is unvalidated | `landed` (`2901a6e9`) — Experimental |

SSDP, LLMNR and NetBIOS-NS are in the **deliberately-silent** class: every
reply is a positive assertion (a device exists / a name maps to an address), so
on LLM failure they must emit *nothing* and record `decision=` in the log the
way `src/server/radius/` does. A fabricated answer poisons a cache.

---

## Wave 2 — clients

All eight of Wave 1 also get clients. Each reuses its server's **existing
Cargo feature**, so no new feature flags — only `src/protocol/client_registry.rs`,
`src/client/mod.rs` and `tests/client/mod.rs` entries.

**Every one of these is unprivileged even where its server is not.** Sending to
UDP 137 or 1900 from an ephemeral source port needs no privilege; only *binding*
the well-known port does.

| Client | Module | What it does | Validation target | Status |
|---|---|---|---|---|
| SSDP | `ssdp` | M-SEARCH discovery of real UPnP devices; surfaces `LOCATION` but deliberately does **not** fetch it (that would turn discovery into an outbound request to an attacker-controlled URL) | A device **emulator** is a live option — see the correction below | `landed` (`29708887`) — Experimental |
| NetBIOS-NS | `netbios_ns` | `nbtstat` equivalent — name query + node status | **Encode is byte-identical to real `nmblookup` queries** (captured with tcpdump, both `NB` and `NBSTAT`). **Decode has no independent evidence** — see the asymmetry note below | `landed` (`41c35f42`) — Experimental |
| NATS | `nats` | Joins a real NATS fabric; LLM reacts to live messages and publishes | **`nats-server` v2.14.6 routes and `async-nats` requests — independent implementations on *both* sides** | `landed` (`a982ccc1`) — **Beta** |
| Ident | `ident` | Queries a remote identd — what an IRC server does. Makes the existing `irc` server able to do genuine ident lookups | **None, structurally**: RFC 1413 has no configurable port, so nothing can be aimed at an ephemeral one | `landed` (`bec74f5d`) — Experimental |
| LLMNR | `llmnr` | Resolves a name via LLMNR multicast; collects a whole window and raises `llmnr_conflicting_responses` when responders disagree | Real Windows / systemd-resolved hosts. **`llmnr-poison` considered and declined — see below** | `landed` (`f415efb4`) — Experimental |
| STOMP | `stomp` | Same shape as NATS, against ActiveMQ/RabbitMQ | Needs a real broker. **The two assertions that would catch a buggy client past a lenient one are written down** in `src/client/stomp/CLAUDE.md` | `landed` (`702eb22f`) — Experimental |
| Gopher | `gopher` | Browses gopherspace; LLM navigates menus | **`geomyidae`/`gophernicus`/`pygopherd` all bind an ordinary port and run fine on loopback** — the external-endpoint ban was never the blocker; none is installed, and a hard-fail (not skip) test would earn Beta | `landed` (`be405db9`) — Experimental |
| Finger | `finger` | Queries a remote finger daemon; **does not parse the response** (RFC 1288 specifies no format) — raw text to the model, with any scraped field marked `GUESS ONLY` | No packaged daemon anywhere (no Homebrew formula for `bsd-finger`/`fingerd`/`netkit`), but a daemon *can* bind a high port, so Beta is achievable in principle | `landed` (`e3f58a57`) — Experimental |

**Evidence can be asymmetric, and the rating must follow the weaker half.**
The NetBIOS-NS client is the worked example: its *encoder* is independently
pinned — the queries it generates are byte-identical to ones real `nmblookup`
4.24.6 puts on the wire — while its *decoder* has met no responder but our own.
It stayed `Experimental`, because Beta's claim is "works against real clients"
and the unproven direction is the one that matters. Note what was *not* done:
stepping down one notch into a rating the same evidence also rules out, which is
the `wireguard` error `CLAUDE.md` records.

Two practical notes from that work, both worth reusing: `tcpdump -i lo0` needs
only `access_bpf` group membership, not root, so capturing a real client's
datagrams is available here; and **hand-transcribing encoded NetBIOS names does
not work** — a 30-character run of `A`s was copied wrong twice, once a byte long
and once a byte short, and both failures read exactly like an encoder bug.
Generate such literals from the pcap with a script.

**Dependency decision: `llmnr-poison` was evaluated and declined (September 2026).**
It is a genuine independent LLMNR encoder — a pure function, depending only on
`anyhow` and `tokio`, so it hand-rolls the DNS wire format and shares no codec
with us. That would have broken the *codec* axis of the LLMNR client's circular
evidence. It was declined anyway, on two grounds that both have to be weighed:

- **Supply chain.** v0.1.0, published three weeks earlier, single author,
  offensive-security purpose, and its stated repository (`icedracon/llmnr-poison`)
  returns 404. A dev-dependency still executes on every developer's machine and in
  CI.
- **It would not have changed the rating.** The *same-project* axis remains — our
  own responder is still the peer — and an independent response *encoder* is not a
  running responder. The protocol stays `Experimental` either way.

A dependency that carries real risk and buys no evidence is a bad trade. If it
matures, has a real repository, and someone wants the codec axis broken, revisit
it — but the rating argument will still not hold on its own.

**A correction worth carrying, from the SSDP pair.** The server-side finding —
"no Rust SSDP crate can be aimed at a unicast loopback port" — is about crates
that *search*, i.e. control points. **It does not generalise to the client half**,
which needs a peer that *answers*, and a device replies to wherever the search
came from. So a device emulator is a live option for the client even though a
client crate was not for the server. Check which *role* a missing-peer finding
applies to before reusing it; the two halves of a protocol have different needs.

**Wave 2 is complete — all eight clients landed.** Seven are `Experimental`
because their only peer is NetGet's own server (same-project evidence: it shows
the two halves agree, not that either matches the spec). **NATS is the exception
and the template**: the official `nats-server` routes while `async-nats`
requests, so both sides are independent implementations.

A pattern worth copying from the STOMP client: rather than just recording "a real
broker would earn Beta", it wrote down **the two specific assertions a lenient
broker would let a buggy client past** — that the delivery was *acknowledged*
rather than redelivered (catching an `ACK` that quotes `message_id` instead of
the `ack` header), and that the session closed on the `RECEIPT` rather than on
the grace timeout. A "what would earn Beta" note is far more useful when it names
the assertions than when it names the software.

Client-specific rules that have bitten this repo before, from `CLAUDE.md`:

- **Do not throw away the model's answer.** The single most common client
  defect, found in six protocols in one pass: `let _ = result.actions`, a bare
  `debug!` of `.len()`, or executing follow-ups through a path that raises no
  event. Box the recursive call and cap it (`MAX_FOLLOWUP_DEPTH`, 4–8).
- **Clients union their action sets** — `client_llm_action_set` is
  async ∪ sync ∪ the firing event's actions. A client cannot express a
  narrowing, so do not try.
- Register **every** spawned task, not just the read loop (BGP's keepalive
  timer kept a socket alive after `remove_client()`).
- Adopt `command_support::register_command_channel` so the dashboard `[send]`
  works, and register it **before** any call that might park on a connect event.

---

## Wave 3 — root and privileged protocols

Thirteen protocols. This is the tier the maintainer considers most interesting,
and the tier where the wireguard trap is most dangerous.

### Tier 1 — L2 impersonation ("NetGet as a rogue switch")

All raw-Ethernet, all `PrivilegeRequirement::RawSockets`, all testable locally
on a `feth` pair under sudo. Pairs with existing `arp` / `datalink` / `isis`,
and shares their `pnet` dependency.

| Protocol | Feature | Module | Wire | What the model decides | Status |
|---|---|---|---|---|---|
| LLDP | `lldp` | `lldp` | EtherType 0x88CC, dest `01:80:C2:00:00:0E` | Chassis/port/system TLVs — impersonate a switch to its neighbour | `landed` (`5e87ba2a`) — Experimental |
| CDP | `cdp` | `cdp` | SNAP, dest `01:00:0C:CC:CC:CC` | Device model, IOS version, native VLAN — the classic recon leak | `planned` |
| STP/RSTP | `stp` | `stp` | 802.3 LLC, dest `01:80:C2:00:00:00` | Bridge priority. Claiming root bridge is a real attack | `landed` (`c4090e37`) — Experimental |
| VRRP + CARP | `vrrp` | `vrrp` | IP proto 112, mcast `224.0.0.18` | Election priority — winning it hijacks the default gateway | `landed` (`88e12101`) — Experimental |
| HSRP | `hsrp` | `hsrp` | UDP 1985, mcast `224.0.0.2` | Same, Cisco's version. **Unprivileged, so the transport genuinely executes** — no HSRP crate exists in any role; only a Cisco device is a real peer | `landed` (`28781187`) — Experimental |
| EAPOL / 802.1X | `eapol` | `eapol` | EtherType 0x888E | Authenticator role. **The strictest fail-closed design here** — see below | `landed` (`5e05313a`) — Experimental |
| Wake-on-LAN | `wol` | `wol` | UDP 9 (or EtherType 0x0842) | Whether a magic packet is honoured, and what is reported | `landed` (`4da604c9`) — Experimental |

**A design consequence LLDP surfaced, which applies to every raw-socket protocol
here.** `privilege_requirement` is a *static* property of the protocol, so a
protocol honestly declaring `RawSockets` is refused by `server_startup` on an
unprivileged host **even when started in UDP test mode**. The test transport is
therefore reachable by calling `Server::spawn` directly — which is what the
suites do, following `bluetooth_ble_beacon` — and not through
`open_server`/MCP. Declaring `None` to dodge that would be a lie about the
transport anyone actually uses, so do not. If this becomes a real obstacle, the
fix is a dynamic privilege check in `server_startup`, not a false declaration.

**VRRP/CARP and HSRP are split deliberately.** VRRP and CARP share a transport
(IP protocol 112) and semantics, so one module with a `variant` startup
parameter is honest. HSRP is UDP — a different transport — and forcing it into
the same module would mean one protocol with two socket types.

**EAPOL is the highest-value item in this tier**: with the existing `radius`
server it forms a complete NAC lab — EAPOL on the wire → RADIUS → the model
decides admission. Build it after or alongside a review of `src/server/radius/`,
whose fail-closed design it must match. An 802.1X authenticator that fails open
is an authentication bypass, which is exactly the OAuth2 defect `CLAUDE.md`
calls the most dangerous pattern in the codebase.

**Wake-on-LAN privilege note:** the UDP form listens on port 9, which is below
1024 — declare `PrivilegedPort(9)`, not `None`. `CLAUDE.md` records `svn`
declaring `PrivilegedPort(3690)`, which can never fire and read as protection
while being dead code; do not invert that mistake here.

**EAPOL is the reference implementation of fail-closed in this repo now**, and
the technique generalises to any protocol where one message *is* an authorisation.
`EAP-Success` cannot be produced except by an explicit model action, guaranteed
four ways: the registry instance holds no request context so it can encode
nothing; `send_eap_success` is refused unless the session already holds an
`EAP-Response/Identity`; the decision layer re-checks the encoded frame so a
regression in that gate is *reported* rather than served; and — the structural
one — **`eapol_eap_success_frame` and `eapol_eap_failure_frame` are eight literal
octets each and share no code**. There is deliberately no
`encode_eap_result(code, id)`, so no boolean exists that could be inverted. Every
fail-closed exit calls the failure builder directly, never the executor.

When its hand-rolled MD5 was later replaced by the `md-5` crate (`008c7261`),
the RFC 1321 and RFC 1994 vector tests were **kept and re-documented** rather
than deleted with the implementation: they no longer claim to be an oracle for
our digest, they guard our *use* of someone else's. A wrapper that feeds the
hasher the wrong buffer, drops an update, or returns the digest with the wrong
endianness would pass every other test in the file and fail those four. Deleting
vector tests along with a hand-rolled primitive is the reflex to resist.

Its own e2e suite then found a real defect the unit tests could not: the model's
`expected_password` was never recorded, so MD5 verification could never succeed.
Being fail-closed, it presented as a stuck exchange rather than a bypass — which
is the whole argument for building it that way round.

**Two lessons from HSRP worth reusing.** First, **state codes that overlap
across versions with different meanings are a silent-corruption trap**: HSRP code
`4` is *Speak* in v1 and *Standby* in v2, both valid, so a mis-decode produces a
plausible wrong answer rather than an error. The fix was to never let a state
cross a version boundary as an integer, have the model see and produce names
only, and pin it from **both** directions — a test in one direction alone passes
with the bug present.

Second, **the fail-closed logging vocabulary is protocol-shaped, not universal**.
`radius` separates `model_reject` from `model_silent` because it has an
Access-Reject to send. HSRP has no negative message at all, so a refusal and an
abstention are the same act and collapse to `model_silent`; what stays
distinguishable — and what actually matters — is that act versus every
`fail_closed_*`. Copy radius's discipline, not its exact token list.

**The single sharpest protocol finding of this programme, from VRRP.**
**CARP is byte-ambiguous with VRRPv2 and misdecodes into the most dangerous
possible message.** Its first octet is `0x21` — a valid VRRPv2 advertisement.
Its `carp_authlen` of 7 lands exactly on VRRP's count-IP-addresses octet, and
`8 + 7*4 == 36` is precisely the CARP header length, so a VRRP decoder accepts a
real CARP packet **without erroring**. Worse, `carp_advskew` of 0 lands on
VRRP's priority field — so a perfectly healthy CARP host decodes as *a master
resigning*, which is the one advertisement that provokes an immediate election.

That is why `variant` is a startup parameter and is **never sniffed**, and why
`a_carp_advertisement_misdecodes_as_a_vrrpv2_resignation` exists to pin it.
Generalise the lesson: when two protocols share a transport, check whether their
headers are *distinguishable* before writing a sniffer — and if they are not,
make the operator say which one, rather than guessing.

A real bug its own CARP e2e caught, worth knowing as a shape: `execute_action`
runs with **no server config in scope**, so it validated a CARP server's action
against the VRRP defaults and rejected `advskew` — the entire CARP path was
unreachable. Anything that validates against configuration must be given that
configuration, or it validates against the wrong thing.

### Tier 2 — IPv6

| Protocol | Feature | Module | Transport | Privilege | Status |
|---|---|---|---|---|---|
| NDP | `ndp` | `ndp` | ICMPv6, raw socket | `RawSockets` | `landed` (`ad5ebe4a`) — Experimental |
| DHCPv6 | `dhcpv6` | `dhcpv6` | UDP 547 | `PrivilegedPort(547)` | `landed` (`2724244c`) — Experimental |

Both belong to the **deliberately-silent** class for the same reason as LLMNR:
an NDP or DHCPv6 answer writes a binding into the peer's stack. Fabricating one
is cache poisoning. Fail silent, log `decision=`.

**The no-storage rule has a consequence worth generalising, which DHCPv6 found
the hard way.** NetGet holds nothing between one model call and the next — so in
any multi-message exchange where the client correlates replies on a
*server-generated* identifier, that identifier must be **deterministic**, not
random. A DHCPv6 client rejects a REPLY whose Server Identifier differs from the
ADVERTISE's, and since the two come from separate model calls with no state
between them, a randomly generated default DUID would break every four-message
exchange. The default is therefore a constant DUID-EN (enterprise 32473, IANA's
reserved documentation number), asserted byte for byte. Ask the same question of
any protocol with a handshake: *what does the peer correlate on, and can we
reproduce it without remembering anything?*

Two `dhcproto` limitations it worked around, recorded so nobody re-discovers
them: `v6::duid::Duid::link_layer` takes the link-layer address as an `Ipv6Addr`
and always writes 16 octets, so it **cannot express a DUID-LL for Ethernet**
(a real one carries a 6-octet MAC) — all four DUID forms are hand-encoded
instead; and it emits the domain search list through a `trust-dns` encoder with
**name compression on**, so a second name sharing a suffix comes back as a
pointer that a client must follow.

> **Router Advertisement was not separately selected, and is nonetheless
> implemented** — RA is ICMPv6 type 134, i.e. part of NDP, so building NDP
> properly delivered it. `send_router_advertisement` carries prefixes with L/A
> flags and both lifetimes, RDNSS (RFC 8106) and MTU, which is the whole
> `mitm6`-class capability. Every RA comes from an explicit model action; there
> is deliberately **no advertisement timer**, so nothing keeps advertising after
> the model stops answering.

**How NDP answered "independent implementation, same author is weak evidence".**
Its checksum literals came from a second one's-complement implementation written
from RFC 8200 §8.1 — which is a weak claim on its own, and it said so. So
`the_checksum_is_reproducible_by_hand` works the smallest case out arithmetically
*in its doc comment* (five addends → `0x18446` → fold → `0x8447` → `0x7bb8`), and
the two terms shown are exactly the two classic mistakes: `0x003a`, the next
header that must be 58, and `0xff04`, the destination address — which is why an
ICMPv6 checksum cannot be computed from the ICMPv6 bytes alone. A reader can
check it without running anything. Copy this wherever a magic constant is the
evidence.

### Tier 3 — become an interface

| Protocol | Feature | Module | Privilege | Status |
|---|---|---|---|---|
| TUN/TAP endpoint | `tuntap` | `tuntap` | `Root` | `landed` (`e73946b9`) — Experimental |
| Raw IP protocol-N | `rawip` | `rawip` | `RawSockets` | `landed` (`a5f40d50`) — Experimental |

**TUN/TAP is the item that changes what NetGet is** rather than adding another
protocol: it takes a real interface, and every routed IP packet becomes an
event, so any L3 service can be synthesized without implementing it.

It has one genuine design problem that must be solved before implementation,
not during: **a per-packet LLM call is hopelessly slow.** Script and static
handlers have to be the default path, with the model reserved for packets a
handler explicitly escalates. Do not ship a version that asks the model per
packet — it will look broken. `CLAUDE.md`'s note that scripts and static
handlers are "the right default for deterministic behavior" is load-bearing here.

**Raw IP protocol-N** is the generic home for protocols with no other (GRE 47,
ESP 50). Keep it genuinely generic; it must not become a per-protocol match.

**Raw IP's genericity is guarded executably, and that is the pattern to copy
whenever a rule is architectural rather than behavioural.** A comment saying "do
not branch on the protocol number" decays; `decoding_does_not_depend_on_the_protocol_number`
does not. It decodes one packet under twelve different protocol numbers and
asserts identical fields *including the payload slice*, so it fails the moment
someone adds `47 => parse_gre()`. Two numeric matches survive and are correct:
the IP **version** nibble (two header formats, not two carried protocols) and a
protocol-number→name string table, which is data rather than logic.

One honest limitation it surfaced and did not paper over: **IPv6 raw sockets do
not deliver the IP header** — the kernel strips it — so on that path the decoder
would see only the payload and drop the packet. Recorded in both its CLAUDE.md
files rather than discovered later by someone debugging live.

### Tier 4 — telecom

| Protocol | Feature | Module | Transport | Privilege | Status |
|---|---|---|---|---|---|
| GTP-C / GTP-U | `gtp` | `gtp` | UDP 2123 / 2152 | **none** — both above 1024 | `landed` (`b1642019`) — Experimental |
| M3UA / SIGTRAN | `m3ua` | `m3ua` | SCTP 2905 | none (port is high) | `landed` (`55f03bb6`) — Experimental; SCTP never executed |

**GTP is the pleasant surprise of this tier**: both its ports are above 1024, so
it needs no privilege and is fully testable here. The model plays an SGW/PGW in
a mobile core. Nobody has an LLM-driven one.

**M3UA is blocked on transport, not on effort.** macOS has no SCTP stack, so its
real transport is unavailable on the machine that would test it. Three ways out,
in order of honesty:

1. Implement over SCTP, and return a clear `Err` naming the missing stack on
   platforms without it — the `bluetooth_ble_beacon` precedent. **Hiding a
   protocol is not the same as refusing to start it**: refused, the operator
   gets `ServerStatus::Error` naming the reason; hidden, nobody learns why.
2. Offer a `transport: tcp` startup parameter for lab use, marked plainly as
   **non-standard** in both the parameter description and `metadata().notes`.
   Some stacks do this; it is not the spec.
3. Userspace SCTP. `webrtc-rs` is already in the tree and carries an SCTP
   implementation, so this is not as far-fetched as it sounds — but it is a
   real project, not a side effect of this one.

Whichever is chosen, M3UA cannot be rated above `Experimental` from macOS.

**M3UA's SCTP handling is the pattern for an unavailable transport.** The
SCTP attempt is a **runtime probe, not a `cfg`**: it asks `socket2` for an
`IPPROTO_SCTP` socket and, on failure, returns an `Err` naming SCTP, RFC 4666,
the escape hatch, and the fact that the escape hatch is non-standard. Three
consequences follow, all of them wanted — the whole path compiles on macOS so it
is type-checked rather than being code nobody builds; a Linux kernel with `sctp`
unloaded is diagnosed identically to macOS; and an operator gets a sentence
rather than an errno.

Equally worth copying: **the non-standard label travels**. `"tcp (NON-STANDARD
lab transport, not SIGTRAN)"` appears in the startup-parameter description the
model reads, `metadata().notes`, the startup log, a standalone WARN, the
dashboard's `protocol_info` row, and **every event's `transport` field** — with a
test asserting each, plus that the declared default is still `"sctp"`, so a
flipped default cannot silently give an SCTP-capable host lab framing.

### Tier 5 — automotive

| Protocol | Feature | Module | Transport | Status |
|---|---|---|---|---|
| CAN bus / SocketCAN | `can` | `can` | `AF_CAN` (Linux) | `landed` (`09b6876d`) — Experimental, Linux transport never compiled |

**Two patterns from CAN worth reusing for any platform-gated protocol.**
First, its refusal test does not stop at asserting `spawn()` returns `Err` — it
then starts the same protocol on its UDP transport on the same host, which proves
the refusal is a **routing decision rather than a dead end**. An assertion that
something fails is much weaker than one showing what still works. Second, the
refusal message is a single `const` quoted verbatim by the runtime error, the
docs and the test, so the three cannot drift.

It also *removed* a startup parameter it had planned (`fd`): `CanFdSocket` reads
both classic and FD frames off one socket, so the knob would have had nothing to
do. Declining to declare a parameter is the cheap way to avoid the
declared-but-unread trap that `startup_param_drift_test` exists to catch.

An LLM-driven ECU simulator, and a genuinely underserved niche. Linux `vcan`
needs **no hardware**, so it is trivially testable on Linux and not at all on
macOS. Same rule as M3UA: implement for Linux, return a clear `Err` naming the
platform elsewhere, and rate honestly. The `socketcan` crate must be a
target-gated optional dependency so non-Linux builds do not try to compile it.

---

## Deliberately deferred

Recorded so they are not re-litigated from scratch.

| Item | Why deferred |
|---|---|
| IPv6 Router Advertisement | Not selected in the September 2026 pass. Recommended; see the Tier 2 note. |
| Transparent intercept (`pf divert-to` / nfqueue) | Most invasive thing considered. Viable but wants its own design pass. |
| Diameter | Pairs with `radius` and works over TCP here, but was not selected. |
| 802.11 rogue AP | macOS cannot inject; needs Linux plus a specific chipset. |
| IPMI/RMCP+, WinRM, DCERPC | Session crypto and IDL marshalling; the model contributes almost nothing for thousands of lines of framing. |
| Redfish | Cheap to build, but the only real clients are Python, and `CLAUDE.md` rules that a generic HTTP client is not Beta evidence — it would be permanently `Experimental`. |

## An unused validation avenue: `tshark`

**`tshark` 4.x is installed on this machine, and it dissects most of what this
programme added.** The GTP agent used it and nobody else did: it rebuilt four of
the server's own outbound packets octet-for-octet from its test assertions,
wrapped them with `text2pcap`, and dissected them. All four parsed with **zero
expert warnings**, including a PN-set/S-clear G-PDU where Wireshark found the
inner IPv4 packet only after all four optional octets — third-party confirmation
of the E/S/PN all-or-nothing rule, from an implementation that shares no code
with ours.

This validates the **encode direction only**, which is exactly the half most of
our raw-socket protocols cannot otherwise prove. It is available today for LLDP,
CDP, STP, VRRP, NDP, DHCPv6, HSRP, WoL, EAPOL and CAN.

It was deliberately **not** wired into the suite, on the reasoning that a test
needing `tshark` would skip when absent, and a skip-when-missing gate is a silent
pass rather than evidence. That reasoning is sound but the conclusion is a
choice, not a necessity: `npm`'s test shows the third option — **hard-fail when
the binary is missing**. The real trade is whether `tshark` becomes a build
requirement wherever the suite runs, exactly as `nats-server` now is. Worth
deciding deliberately rather than by default; it would move several protocols
from "codec proven only against ourselves" to "codec proven against Wireshark".

## A new build requirement: `nats-server`

The NATS **client**'s Beta rating rests on a test that spawns the real
`nats-server` binary (v2.14.6, installed September 2026) and hard-fails when it
is absent — deliberately, because a skip-when-missing gate is a silent pass and
is not evidence. **So `nats-server` must exist wherever that suite runs.**

Today this costs nothing: `ci.yml`'s `test` job builds
`tcp,http,dns,udp,redis,mcp-stdio`, which does not include `nats`, so the test
is never compiled there. **Anyone adding `nats` to the CI feature set must also
install `nats-server` in that job**, or the build will fail — correctly, and
loudly, which is the intended behaviour.

The same will apply to any protocol promoted by the "install the real client"
route: Gopher would need `geomyidae`/`gophernicus`, Finger a real daemon, LLDP
`lldpd`. That is the price of the Beta bar, and it is the right price.

## Follow-ups this programme created

Each was found by an agent working inside its own boundary, reported rather than
reached outside to fix, and is small. Do them once the waves have landed, not
during — every one touches a shared file.

1. **`src/tui/wireshark.rs` has no entry for any protocol added here.**
   Two concrete instances confirmed by agents: DHCPv6 (`wireshark.rs:203` has
   `"dhcp" | "bootp" => udp("dhcp")` and no `dhcpv6` arm) and Wake-on-LAN.
   Both are one-line additions. That table
   maps NetGet protocol → transport + Wireshark dissector, and a protocol missing
   from it is treated as plain TCP. That is wrong for every UDP and link-layer
   protocol in this programme. Wake-on-LAN found it (it is UDP/9 with the `wol`
   dissector, not TCP); the same applies to SSDP, LLMNR, NetBIOS-NS, HSRP, GTP,
   DHCPv6, NDP, VRRP, LLDP, CDP, STP, EAPOL and CAN. Every name added must be
   checked against `tshark -d` / `-Y`, as the existing table's entries were.
2. **The privilege gate is per-protocol, not per-transport.** Confirmed
   independently by the LLDP and STP agents. `server_startup` evaluates
   `privilege_requirement` *before* reading startup parameters, so a protocol
   honestly declaring `RawSockets` is refused on an unprivileged host **even when
   asked for its UDP test transport**. The suites work around it by calling
   `Server::spawn` directly, following `bluetooth_ble_beacon`. The fix is a
   per-transport privilege query in `server_startup`; the wrong fix is declaring
   `None`, which would lie about the transport anyone actually uses.
3. **`CLAUDE.md`'s "Running tests" example is not a valid cargo invocation, and
   this file repeated it.** It shows `--test server::tcp::e2e_test`, but `--test`
   names a test *target* — a file in `tests/` — so the target is `server` and the
   rest is a filter. Cargo lists the available targets and exits rather than
   running anything, which reads like a broken suite. The working form is:

   ```bash
   ./cargo-isolated.sh test --no-default-features --features tcp \
       --test server -- server::tcp --test-threads=100
   ```

   Every agent in this programme was briefed with the wrong form and each worked
   it out independently. Fix the example in `CLAUDE.md`.
4. **`validate_static_action_names` never reads a client's async actions.**
   Found by the Ident client agent. `src/events/handler.rs` builds its catalogue
   from `get_sync_actions()` plus event-type actions only — so any client that
   follows the "clients union, don't duplicate into both methods" rule **cannot be
   routed with a static handler**, failing with *"Unknown action … Valid actions
   for X: append_memory, …"*. **`whois` has this bug today**: its own
   `get_startup_examples()` static example cannot be created. The ~40 clients that
   appear to work do so by duplicating their whole list into `get_sync_actions()`.
   The fix is a one-line change to teach the validator to read async actions for
   clients; the local workaround is to attach the vocabulary via
   `EventType::with_actions(...)`, which is what that field is for.
5. **Trim unused feature dependencies.** `stp` declares `pnet` and does not use it
   (its codec is hand-written; `pcap` covers capture and injection). Check `lldp`,
   `cdp` and `eapol` for the same once they land — I declared all four alike when
   wiring, before knowing which would need it. A declared-but-unused dependency is
   build weight and a false signal about how a protocol works.

## Working rules for this programme

Learned from the incidents recorded in `CLAUDE.md`, and applied to every wave:

1. **Wire shared files centrally, before fanning out.** `Cargo.toml`, both
   registries, `src/server/mod.rs`, `src/client/mod.rs` and both test `mod.rs`
   files are edited by one actor only. Feature gating means an agent can build
   and test its own protocol alone even though the others do not exist yet.
2. **Each agent owns exactly two directories** and commits them through a
   private index (`GIT_INDEX_FILE`) with a compare-and-swap `update-ref`. The
   shared index races; five separate incidents are on record.
3. **Stage the waves.** Do not run twenty-plus agents at once — they serialize
   on the shared `target/` build lock, and a large wave during a degraded API
   period once burned ~9.8M tokens for 5 usable results.
4. **A validation client's licence is a dev-dependency question, not a shipping
   question.** NetGet is `AGPL-3.0-or-later` (`Cargo.toml` line 5). A test-only
   client is never linked into a distributed binary, which is the same basis on
   which the MPL-2.0 `irc` and `attohttpc` test dependencies are already here.
   `async-stomp` is EUPL-1.2 and was cleared on that ground *and* because
   EUPL-1.2's compatibility appendix names AGPL-3.0 explicitly. Escalate a
   copyleft **runtime** dependency; a dev-dependency usually just needs the
   reasoning written down.

   **`LICENSE_ANALYSIS.md` contradicts itself and the manifest** and should be
   re-derived before anyone relies on it: it claims "No copyleft obligations
   (can keep code proprietary if desired)" a few lines after answering "Can
   NetGet be closed-source? **No**", and it describes a project that is not
   AGPL. Not fixed here; recorded so the next licence question starts from the
   manifest rather than from that file.
5. **Watch `df` between waves.** `target/` reached 130 GB in one session, and at
   zero bytes free the session cannot recover — every tool call needs to write.
   `cargo clean --profile dev` is the remedy and keeps `target/release`.

---

# Programme 2 — quality pass over every protocol

Started 4 September 2026. Goal: every one of the 154 servers and 102 clients
audited and improved for robustness, failure semantics, model-facing accuracy,
lifecycle correctness, test honesty and documentation truth.

**Paused mid-flight on quota exhaustion.** Read "Where to resume" below.

## How the work is organised

39 family batches, one agent each, run in waves of about ten. Each agent works
in **its own git worktree** with a **shared `CARGO_TARGET_DIR`**
(`/Users/matus/dev/netget-shared-target`) — worktrees give git isolation, the
shared target directory stops 39 private `target/` trees refilling the disk.

**`src/tui/wireshark.rs` is deliberately excluded from every agent's boundary.**
Agents report the correct entry for their protocols; it is applied centrally.
One table edited by 39 agents is the collision this repo has already hit.

## The eight-point rubric each agent applies

1. **Robustness** — hostile input must not panic, hang or allocate without bound:
   unbounded line/message reads, missing read timeouts, byte-index slicing on a
   `&str` (use `crate::utils::truncate_for_log`), a lock guard held across an
   `.await` doing I/O or an LLM call, `block_on`/`blocking_lock()` on the async
   runtime, and library panics reachable from the wire.
2. **Failure semantics** — a `crate::utils::wire_failure` category to the peer
   (never the error text), or deliberate silence where every reply the protocol
   defines is a positive assertion; `decision=` log tags as `src/server/radius/`.
3. **Model-facing surface** — action descriptions match executor behaviour, every
   declared `example` accepted by its own executor, no raw bytes or base64,
   `.with_actions(...)` on every event, every declared event actually emitted.
4. **Startup parameters** — declared ⇄ read both ways; `?` never `unwrap()`.
5. **Lifecycle** — `spawn()` awaits readiness and returns `Err`; every spawned
   task registered; connection stats updated; `.connectionless()` where apt.
6. **Tests** — `verify_mocks()` everywhere; wait for *conditions*, never fixed
   sleeps; an `#[ignore]`d or skip-when-missing test is not evidence.
7. **Docs and honesty** — `CLAUDE.md` matches the code; **lower** any maturity
   rating the evidence does not support and say why.
8. **Wireshark** — report the entry, do not edit the file.

**Prefer removing a defect or a lie over adding a feature.** "This one is already
sound" is a good outcome; inventing work to look productive is not. Never weaken
a test to make it pass.

## Where to resume

- **Wave 1 (pilot) was in flight when quota ran out**: `mail` (smtp/imap/pop3/nntp),
  `l2raw` (arp/datalink/icmp/igmp), `db-core` (mysql/postgresql/redis/memcached).
  Their worktrees may hold committed or partial work — **check
  `git worktree list` and each worktree's log before re-running them**, and merge
  anything already committed rather than redoing it.
- The pilot existed to validate two unknowns: the worktree mechanics (where it
  lives, what branch, how commits come back) and whether the rubric yields real
  fixes rather than busywork. **Answer those before launching the remaining 36.**
- Everything after the pilot is unstarted.

## Two operational corrections learned in wave 2

**A shared `CARGO_TARGET_DIR` corrupts binary-spawning test suites.** It was the
right call for disk (39 private target trees would refill the disk), but
`tests/examples`, `tests/terminal_snapshot` and every suite that spawns
`target/debug/netget` read a binary another agent may be rebuilding *right now*.
The DNS batch saw ten client tests fail with `Client protocol 'DNS' exists but is
not compiled into this build`; all ten passed unchanged in a private target dir.

So: **share the target dir for ordinary builds, but verify binary-spawning suites
in a private one** — `CARGO_TARGET_DIR=/tmp/verify-<batch>`. Treat any
"protocol exists but is not compiled in" failure during a wave as contention
until proven otherwise; it is an artefact, not a regression.

**A blocking `std::process::Command::output()` deadlocks the in-process mock
model.** The test's current-thread runtime is shared with the mock Ollama server,
so a blocking `dig` waits for a model that cannot run until `dig` returns. It
presents as `;; connection timed out` against a perfectly healthy server. Use the
async spawn. Related: `start_netget_server` returns when startup is *parsed*, not
when the socket is bound.

## Wave 4/5 — cut short by a second session limit, work preserved

Six agents were killed mid-task on 11 September. **Every one had committed real fixes first**,
and those were merged; each agent's final in-flight edits were preserved by the supervising
session as a `wip(...)` commit at the tip of its branch and **deliberately excluded from the
merge**. The precedent for that caution is the previous wave, whose `wip` commit contained a
helper referencing a function that existed nowhere and could never have compiled.

| batch | branch | merged | unverified WIP left on branch |
|---|---|---|---|
| core-transport | `worktree-agent-a3e18a8987266e2bc` | 4 commits | `00883c69` |
| netmgmt | `worktree-agent-a49600538a485057e` | 2 commits | `6293ce0c` |
| chat | `worktree-agent-a48f49cf447d93de7` | 1 commit | `eb6cc6bb` |
| tls-vpn | `worktree-agent-a357b18f2ed735606` | 4 commits | `dd7f5d41` |
| web-aux | `worktree-agent-a09462d7c51094ccf` | 8 commits | `7d408691` |
| auth-web | `worktree-agent-aa3a5bf60938d9bda` | **none** | `07b85160` — all its work is unverified |

**To resume**: `git merge --no-ff worktree-agent-<id>` picks up the WIP too, so verify it first.
`auth-web` should simply be re-run — it committed nothing, and it is the family containing the
OAuth2 fail-open that `CLAUDE.md` names as the worst defect in the codebase's history.

**The stack-overflow class has now been found in four protocols**: AMQP, NATS and STOMP in the
messaging family, and SNMP's BER decoder here — *"one 60 KB datagram kills the whole process"*.
It is worth checking any remaining decoder that can call itself.

## The 39 batches

```
# batch-name | protocols (server+client together)
PILOT-1 mail        | smtp imap pop3 nntp
PILOT-2 l2raw       | arp datalink icmp igmp
PILOT-3 db-core     | mysql postgresql redis memcached
web-core            | http http2 http3 http_common quic
web-aux             | websocket webdav proxy http_proxy socks5
dns                 | dns doh dot mdns
db-nosql            | mongodb cassandra couchdb elasticsearch
db-cloud            | dynamo s3 sqs snowflake spark
db-enterprise       | mssql db2 oracle etcd zookeeper
messaging           | amqp mqtt kafka nats stomp
routing             | bgp ospf rip isis
redundancy          | vrrp hsrp stp lldp cdp
discovery           | ssdp llmnr netbios_ns
voip                | sip rtp rtsp hls
webrtc              | webrtc webrtc_signaling stun turn
files               | ftp tftp nfs smb
vcs                 | git svn mercurial
packages            | npm pypi maven yarn oci_registry
auth-dir            | ldap radius ssh ssh_agent
auth-web            | oauth2 openid saml_idp saml_sp eapol
tls-vpn             | tls ipsec openvpn wireguard
legacy-text         | whois finger gopher ident telnet
core-transport      | tcp udp dc reverse_shell
ipc                 | named_pipe pty stdio socket_file
rpc                 | jsonrpc xmlrpc grpc mcp
llm-api             | openai ollama openapi
chat                | irc xmpp
p2p                 | bitcoin torrent_dht torrent_peer torrent_tracker tor_relay
iot-industrial      | modbus coap can gtp m3ua
netmgmt             | snmp syslog ntp dhcp bootp dhcpv6
raw-new             | ndp rawip tuntap wol
remote-desktop      | vnc rdp
print-feed          | ipp rss
k8s                 | kubernetes
usb                 | usb (7 sub-protocols)
ble-core            | bluetooth_ble bluetooth_ble_beacon
ble-hid              | bluetooth_ble_keyboard bluetooth_ble_mouse bluetooth_ble_gamepad bluetooth_ble_presenter bluetooth_ble_remote
ble-sensors          | bluetooth_ble_battery bluetooth_ble_heart_rate bluetooth_ble_thermometer bluetooth_ble_environmental bluetooth_ble_proximity bluetooth_ble_weight_scale bluetooth_ble_cycling bluetooth_ble_running bluetooth_ble_data_stream bluetooth_ble_file_transfer
nfc                 | nfc
```

Client directories whose name differs from the server's — the batch owning the
server also owns these: `bluetooth` (ble-core), `dynamodb` (db-cloud),
`openidconnect` and `saml` (auth-web), `tor` (p2p).

## Worktree mechanics — answered, no need to re-derive

Agent worktrees live at `.claude/worktrees/agent-<id>`, each on its own branch
`worktree-agent-<id>`, branched from the master commit at launch. They are
**locked**, so `git worktree prune` will not remove them. Merge with
`git merge --no-ff <branch>` from the main tree; per-protocol directories are
disjoint, so conflicts should be limited to shared files agents were told not
to touch.

Wave 1 ran until a session limit killed all three agents mid-task. **Every one
had produced real fixes first**, and the pilot therefore did its job: it proved
the rubric finds genuine defects rather than generating busywork.

| batch | branch | verified commit | unverified WIP |
|---|---|---|---|
| mail | `worktree-agent-a26377dd9a0f3624b` | `fa9f7775` — a mutex guard in a `match` scrutinee deadlocked the NNTP client read loop | `616f818d` (smtp) |
| l2raw | `worktree-agent-ae9489544f89cd1ea` | `0ce7a3bc` — ARP: a client deadlock, a discarded model answer, a leaked capture handle, and a missing fail-closed log | `4da520ba` (icmp) |
| db-core | `worktree-agent-afc24a51591969272` | `606d33a5` — MySQL prepared statements never worked: three defects and the docs that hid them | `6430977f` (postgresql) |

**The `wip(...)` commits were made by the supervising session, not by the agents,
and were never built or tested.** They exist so the work is not lost, not because
they are believed correct. Verify before merging; treat them as a starting point.

The three named fixes are the agents' own commits and were made in the normal
way, but **none has been verified by a build in the main tree** — the session
lost its quota before that could happen. Build and test each before merging.

Note the `mail` branch also merged master into itself, so a diff against the old
base shows 249 commits; only `fa9f7775` and `616f818d` are its own work.

**Agent worktrees branch from `origin/master`, NOT from local `HEAD`. Push
before launching them.** This is the most expensive thing wave 1 taught, and it
is not obvious.

Local master was 249 commits ahead of the remote when wave 1 launched, so all
three worktrees were created at `525029e1` — a base predating every one of those
commits. The `mail` agent noticed and merged master into its worktree first,
which is why it merged back cleanly. The other two did not, and re-fixed ground
master had already covered: `687c4c7a` had made every documented example
executable, and `176fafe8`/`20f36994` had already reworked ARP. Merging them back
produced semantic conflicts between two independent fixes to the same lines —
the worst kind to resolve, because both sides are *correct* and neither is
simply newer.

The rule is now in `CLAUDE.md`: **pushing to `origin` is pre-authorised in this
repo, so push as you go.** Before launching any worktree wave, verify
`git rev-list --count origin/master..HEAD` is `0`.

**What the pilot settled:** the worktree mechanics (above), and that the rubric
yields real defects — a deadlock, a discarded answer, a resource leak and a
whole broken feature, inside three families in one short wave. The remaining 36
batches can proceed on the same pattern.

Check each with `git -C .claude/worktrees/agent-<id> log --oneline` and
`git -C .claude/worktrees/agent-<id> status` before deciding — a worktree at
base may still hold uncommitted work that is worth keeping.
