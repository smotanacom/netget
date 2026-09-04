# NetGet Protocol Roadmap

Durable tracking for the protocol-expansion programme started September 2026:
what we decided to build, why, and where each item stands.

**This is not a status report.** The root `CLAUDE.md` warns against adding
one-off session/status files — that is what let the root directory reach 63
markdown files. This is one file, updated *in place* as items move, and it is
meant to be edited rather than superseded. If an item lands, change its row;
do not write a new file about it. When every row here reads `landed`, fold the
durable lessons into `CLAUDE.md` and delete this file.

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
| NetBIOS-NS | `netbios_ns` | `nbtstat` equivalent — name query + node status | Real Windows/Samba hosts; no local `nmbd` installed | `planned` |
| NATS | `nats` | Joins a real NATS fabric; LLM reacts to live messages and publishes | **`nats-server` via brew — non-circular, deterministic, real Beta path** | `planned` |
| Ident | `ident` | Queries a remote identd — what an IRC server does. Makes the existing `irc` server able to do genuine ident lookups | **None, structurally**: RFC 1413 has no configurable port, so nothing can be aimed at an ephemeral one | `landed` (`bec74f5d`) — Experimental |
| LLMNR | `llmnr` | Resolves a name via LLMNR multicast | Real Windows / systemd-resolved hosts | `planned` |
| STOMP | `stomp` | Same shape as NATS, against ActiveMQ/RabbitMQ | Needs a broker (Docker, or brew + STOMP plugin) | `planned` |
| Gopher | `gopher` | Browses gopherspace; LLM navigates menus | **`geomyidae`/`gophernicus`/`pygopherd` all bind an ordinary port and run fine on loopback** — the external-endpoint ban was never the blocker; none is installed, and a hard-fail (not skip) test would earn Beta | `landed` (`be405db9`) — Experimental |
| Finger | `finger` | Queries a remote finger daemon; **does not parse the response** (RFC 1288 specifies no format) — raw text to the model, with any scraped field marked `GUESS ONLY` | No packaged daemon anywhere (no Homebrew formula for `bsd-finger`/`fingerd`/`netkit`), but a daemon *can* bind a high port, so Beta is achievable in principle | `landed` (`e3f58a57`) — Experimental |

**A correction worth carrying, from the SSDP pair.** The server-side finding —
"no Rust SSDP crate can be aimed at a unicast loopback port" — is about crates
that *search*, i.e. control points. **It does not generalise to the client half**,
which needs a peer that *answers*, and a device replies to wherever the search
came from. So a device emulator is a live option for the client even though a
client crate was not for the server. Check which *role* a missing-peer finding
applies to before reusing it; the two halves of a protocol have different needs.

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
| VRRP + CARP | `vrrp` | `vrrp` | IP proto 112, mcast `224.0.0.18` | Election priority — winning it hijacks the default gateway | `planned` |
| HSRP | `hsrp` | `hsrp` | UDP 1985, mcast `224.0.0.2` | Same, Cisco's version | `planned` |
| EAPOL / 802.1X | `eapol` | `eapol` | EtherType 0x888E | Supplicant **and** authenticator | `planned` |
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

### Tier 2 — IPv6

| Protocol | Feature | Module | Transport | Privilege | Status |
|---|---|---|---|---|---|
| NDP | `ndp` | `ndp` | ICMPv6, raw socket | `RawSockets` | `planned` |
| DHCPv6 | `dhcpv6` | `dhcpv6` | UDP 547 | `PrivilegedPort(547)` | `planned` |

Both belong to the **deliberately-silent** class for the same reason as LLMNR:
an NDP or DHCPv6 answer writes a binding into the peer's stack. Fabricating one
is cache poisoning. Fail silent, log `decision=`.

> **Not selected but recommended:** IPv6 **Router Advertisement**. It was the
> highest-impact item of this tier — a rogue RA reroutes an entire LAN's IPv6
> (`mitm6` with reasoning) — and it is the natural partner of both rows above.
> Recorded here so the decision is deliberate rather than forgotten.

### Tier 3 — become an interface

| Protocol | Feature | Module | Privilege | Status |
|---|---|---|---|---|
| TUN/TAP endpoint | `tuntap` | `tuntap` | `Root` | `planned` |
| Raw IP protocol-N | `rawip` | `rawip` | `RawSockets` | `planned` |

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

### Tier 4 — telecom

| Protocol | Feature | Module | Transport | Privilege | Status |
|---|---|---|---|---|---|
| GTP-C / GTP-U | `gtp` | `gtp` | UDP 2123 / 2152 | **none** — both above 1024 | `planned` |
| M3UA / SIGTRAN | `m3ua` | `m3ua` | SCTP 2905 | none (port is high) | `blocked` on macOS — see below |

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

### Tier 5 — automotive

| Protocol | Feature | Module | Transport | Status |
|---|---|---|---|---|
| CAN bus / SocketCAN | `can` | `can` | `AF_CAN` (Linux) | `planned`, Linux-only |

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

## Follow-ups this programme created

Each was found by an agent working inside its own boundary, reported rather than
reached outside to fix, and is small. Do them once the waves have landed, not
during — every one touches a shared file.

1. **`src/tui/wireshark.rs` has no entry for any protocol added here.** That table
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
