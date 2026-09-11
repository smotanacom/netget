# NDP Protocol Implementation

IPv6 Neighbour Discovery (RFC 4861): the protocol that replaced ARP, ICMP Router Discovery and
ICMP Redirect all at once. Five ICMPv6 messages carry the whole thing.

**State**: `Experimental`, and the reason is specific — see [Maturity](#maturity-and-what-would-earn-beta).
**Privilege**: `PrivilegeRequirement::RawSockets`.
**Connectionless**: declared, so the 10-second idle sweep reaps the per-peer entries.
**Stack**: `IPv6>ICMPv6>NDP`. ICMPv6 types 133–137, code 0, Hop Limit 255.
**Spec**: [RFC 4861](https://www.rfc-editor.org/rfc/rfc4861), [RFC 4443](https://www.rfc-editor.org/rfc/rfc4443)
(ICMPv6 + checksum), [RFC 8200 §8.1](https://www.rfc-editor.org/rfc/rfc8200#section-8.1)
(the pseudo-header), [RFC 8106](https://www.rfc-editor.org/rfc/rfc8106) (RDNSS),
[RFC 4291 §2.7.1](https://www.rfc-editor.org/rfc/rfc4291#section-2.7.1) (solicited-node multicast).

## The Router Advertisement is why this protocol is here

Say it plainly, because it is the whole point and it is also the whole danger.

**A Router Advertisement hands an entire link its configuration.** One message carries the prefix
hosts build their addresses from, the default route they send everything through, the link MTU,
and — via RDNSS — the DNS resolvers they resolve every name with. Every host on the segment
accepts it from anybody, unauthenticated, because that is what the protocol says to do. A rogue
one is not a routing nuisance; it is a **complete traffic redirect**, which is exactly what
`mitm6` does.

The other four messages are smaller versions of the same property. A Neighbour Advertisement
writes "this IPv6 address is at this link-layer address" into the peer's neighbour cache and the
peer then sends that address's traffic there. A Redirect rewrites one route.

So: **a model authoring a Router Advertisement is the interesting thing this protocol does**, and
it is also the reason the failure path is silence rather than a plausible default. Use it on
links you own or have permission to test.

## Files

| File | What it is |
|---|---|
| `codec.rs` | The **pure** RFC 4861 codec. No socket, no config, no global state. This is the part that is proven. |
| `actions.rs` | `Protocol` + `Server`: four actions, five events, metadata, startup parameters. |
| `mod.rs` | Transport. Two of them, both thin over `codec.rs`. |

### The split is the point

The raw transport needs root or `CAP_NET_RAW`, which nothing in this repository has, so it can
never be executed here. The packet format can be, and is — against literal specification bytes in
`tests/server/ndp/codec_test.rs`. This is the `bluetooth_ble_beacon` precedent the root
`CLAUDE.md` describes: *"payload construction is pure and exhaustively unit-tested against literal
spec bytes; its BlueZ transport has never been compiled or run"*. `metadata().notes` says which
half is which, which is what `Experimental` is for.

Keep `codec.rs` pure. The moment it reads a socket or a config value, the only testable part of
this protocol stops being testable.

## The two things NDP implementations get wrong

Both are pinned against literal bytes, because both produce packets that look fine in a debugger
and that every conforming receiver silently drops.

### 1. Option length is in units of 8 octets

RFC 4861 §4.6. A Prefix Information option occupies **32 octets** and its length field reads
**4**. Writing `32` there makes every receiver walk 256 octets, past the end of the packet, and
desynchronises the option walk for everything after it.

| Option | Octets | Length field |
|---|---|---|
| Source/Target Link-Layer Address (1/2) | 8 | 1 |
| Prefix Information (3) | 32 | 4 |
| MTU (5) | 8 | 1 |
| RDNSS (25) | 8 + 16n | 1 + 2n |

`NdpOption::encode` refuses to produce anything that is not a whole number of units, and
`decode_options` refuses a length of **zero** — RFC 4861 §4.6 requires that, and it is not
pedantry: a zero length is an infinite loop in a naive walker and is how a hostile neighbour
wedges a stack.

### 2. The ICMPv6 checksum covers an IPv6 pseudo-header

RFC 4443 §2.3 with RFC 8200 §8.1. The 40-octet pseudo-header — **never transmitted** — is:

```text
| source address (16) | destination address (16) | upper-layer length (4) | zero(3) | 58 |
```

So the checksum is **not computable from the ICMPv6 bytes alone**, and every encode entry point
on `NdpMessage` takes both addresses as required arguments rather than options. The two classic
mistakes are omitting the pseudo-header entirely and using the *preceding* header's next-header
value instead of 58.

`codec_test.rs::the_checksum_is_reproducible_by_hand` works the smallest case out arithmetically
in its doc comment, so a reader can check the scheme without running anything, and
`the_checksum_depends_on_both_addresses` asserts that a checksum over the message alone is a
different number.

**One honesty note about the raw path.** RFC 3542 §3.1 makes the kernel compute and insert the
ICMPv6 checksum for an `IPPROTO_ICMPV6` raw socket, so on that transport our own calculation is
overwritten. It is load-bearing on the UDP test transport (where the test verifies it) and in the
codec tests. `encode_without_checksum` exists so the raw path can be honest about that.

## Transports

### `transport: "raw"` (default)

A raw ICMPv6 socket via socket2, with the **unicast and multicast** hop limits both set to 255
— which is two *outbound* settings, not one in each direction. RFC 4861 §11.2 makes the hop
limit NDP's entire defence against an off-link attacker (a router decrements, so 255 on arrival
proves one hop), and this implements only the sending half of it: nothing inspects the hop limit
of a message that *arrives*. Doing so needs `IPV6_RECVHOPLIMIT` and a cmsg read, which is the
same gap as the `IPV6_RECVPKTINFO` one listed below.

This paragraph read "hop limits set to 255 in both directions" until this pass, which says the
receiver check is implemented. It never was, and it is the half that does the defending — worth
recording because the wording was persuasive enough to survive several readings.

**Never executed anywhere.** It compiles, and it is written to the same shape as `icmp` and
`lldp`, including the parts those learned the hard way:

- **Startup reports failure.** The socket is opened inside `spawn_blocking`, but the outcome comes
  back over a `oneshot` and `spawn()` only returns `Ok` once it is genuinely open. ARP, DataLink
  and ICMP each shipped the fire-and-forget version and were each fixed separately.
- **The interface is resolved *before* the socket**, and a missing one refuses the start. NDP is
  link-local: a raw ICMPv6 socket with no scope id cannot send to `ff02::1` at all, so opening one
  first would produce a server that starts and can never transmit — the same class of lie.
- **Stopping is cooperative.** `JoinHandle::abort()` cannot interrupt a thread parked in a
  blocking receive, so the loop polls a `StopSignal` and the registered task trips it. The socket
  is non-blocking and the loop sleeps 10ms on `WouldBlock`.
- **A raw IPv6 socket never includes the IPv6 header** (RFC 3542 §3), so what arrives is the
  ICMPv6 message itself — no header stripping, unlike `icmp`'s IPv4 path.

Known limitations of this path, all unexercised:

- **The source address is the kernel's choice, not `link_local_address`.** Nothing binds the
  socket, so the parameter is used for our checksum (overwritten) and for reporting. Binding a
  link-local address requires a scope id and has not been attempted.
- **The received destination address is unknown** without `IPV6_RECVPKTINFO`, so `verify_checksum`
  is not called on that path. The kernel has already verified it, so nothing is lost — but it does
  mean the checksum code is exercised end to end only on the UDP transport.
- **No ICMP6_FILTER.** The socket receives every ICMPv6 message on the host; anything that is not
  one of the five types is dropped at `NdpMessage::decode` with a `trace!`.
- **The inbound Hop Limit is not checked.** RFC 4861 §11.2 requires a receiver to discard an NDP
  message that did not arrive with 255, and that check is the protocol's only defence against an
  off-link attacker. It needs `IPV6_RECVHOPLIMIT` plus a cmsg read, which this socket does not
  request — the same gap as `IPV6_RECVPKTINFO` above, and a more consequential one.

### `transport: "udp"` (testing)

Datagrams carrying `source(16) || destination(16) || ICMPv6 message`.

This is the `ospf` accommodation (`Cargo.toml`: *"requires root/CAP_NET_RAW, tests use UDP"*) made
**explicit as a declared startup parameter** rather than left to the test file — `ospf`'s E2E
suite starts a plain UDP server and hand-rolls packets, so it never touches `ospf`'s own code at
all.

The framing carries the two addresses **on purpose**: they are exactly the part of the IPv6 header
the checksum depends on, so the receiving side can verify it. A test transport that carried only
the ICMPv6 bytes would leave the single most error-prone thing in the protocol unexercised end to
end, which would defeat the reason for having a test transport.

No real IPv6 stack speaks it, and the parameter description says so.

**One consequence worth knowing**: because `metadata()` declares `RawSockets`, `server_startup`
refuses to start NDP at all on an unprivileged host — *including* in UDP mode, since privilege is
a static property of the protocol and cannot depend on a parameter. So the UDP transport is
reachable by calling `Server::spawn` directly (which is what the test suite does) and not through
`open_server`/MCP without privilege. Declaring `None` to work around that would be a lie about the
transport anyone actually uses.

## No advertisement timer, deliberately

A real router advertises unprompted every few minutes. **This server does not, and should not be
given the option.** An unsolicited Router Advertisement reconfigures every host that hears it, and
a NetGet instance left running with a timer would keep doing so to a link nobody asked it to
touch. Every advertisement here comes from an explicit model action in response to something
received.

(`lldp` does have such a timer, and that difference is the right one: an LLDP advertisement adds a
row to a table an operator reads, while an RA changes how a machine routes.)

## Startup parameters

Four declared, four read, all in `NdpServer::spawn_with_llm_actions`.

| Parameter | Default | What reads it |
|---|---|---|
| `transport` | `"raw"` | Selects `spawn_raw` or `spawn_udp` |
| `udp_peer` | last peer heard from | Destination for outgoing datagrams. **Rejected with `transport: "raw"`**, where it would do nothing |
| `link_local_address` | `fe80::1` | Source of every message we send — and therefore what a host installs as its default router — and half of every pseudo-header |
| `link_layer_address` | the interface's own address, else `02:00:00:00:00:01` | Fills the Source/Target Link-Layer Address option when an action names none |

`link_layer_address`'s fallback uses `pnet::datalink::interfaces()`, which also supplies the
interface index used as the IPv6 scope id.

## What the model sees and controls

### Events

| Event | When |
|---|---|
| `ndp_router_solicitation` | A host asked for a router. The opening for everything above. |
| `ndp_router_advertisement_received` | Another router configured this link; prefixes, MTU and RDNSS already decoded |
| `ndp_neighbor_solicitation` | Somebody asked where an IPv6 address lives |
| `ndp_neighbor_advertisement` | Somebody answered — or announced themselves unsolicited |
| `ndp_redirect_received` | A router moved a destination to a different first hop |

All five declare the full action set with `.with_actions(...)` and all five have real emit sites in
`handle_message`. `tests/server/ndp/e2e_test.rs::every_message_type_raises_its_own_event` drives
one of each through the transport and requires the matching `decision=` line, which names the
event id — a static-source check cannot tell which decoded message maps to which event.

Event data is addresses as IPv6 strings, link-layer addresses as `"00:11:22:33:44:55"`, flags as
booleans, lifetimes as numbers of seconds. Nothing byte-shaped, and
`codec_test.rs::event_data_carries_no_octets_anywhere` walks every key and value to keep it that
way. Fields a message did not carry are **absent**, not `null`, so a script can test with a plain
`in`.

### Actions

| Action | Effect |
|---|---|
| `send_router_advertisement` | Prefixes (with L/A flags and both lifetimes), RDNSS, MTU, M/O flags, router lifetime, hop limit |
| `send_neighbor_advertisement` | Claim an address, with R/S/O flags and a target link-layer address |
| `send_neighbor_solicitation` | Ask where an address lives. The only *question* here; everything else asserts |
| `no_response` | Say nothing, deliberately. Logged `decision=model_reject` |

Every one is validated **at the action** by running the encoder, so the model is told rather than
a host silently discarding the message. Three refusals worth knowing, each chosen because the
alternative is a silent no-op on the wire:

- **An autonomous prefix must be a `/64`.** SLAAC appends a 64-bit interface identifier, so a
  `/48` with the A flag leaves no room and every host ignores the option. On-link-only prefixes of
  any length are fine.
- **`preferred_lifetime` must not exceed `valid_lifetime`** — RFC 4861 §4.6.2 requires a host to
  ignore the whole option when it does.
- **An MTU below 1280** is below IPv6's minimum link MTU (RFC 8200 §5).

Two defaults worth knowing:

- **A Neighbour Advertisement defaults to solicited + override**, because that is what an answer
  to a solicitation is. `router` defaults to false: claiming to be a router is a decision.
- **The server fills in its own link-layer address** when the action names none. RFC 4861 §4.4
  requires the option on a solicited advertisement; the model does not know our hardware address
  and should not have to.
- **CIDR notation is accepted** (`"2001:db8:1::/64"`), because that is how prefixes are written
  everywhere else. An explicit `length` wins.

Default destinations follow RFC 4861: an answer goes back to whoever asked; a solicitation goes to
the **target's** solicited-node multicast group (`ff02::1:ffXX:XXXX`), not back to the peer; and a
peer whose source address was `::` — a node doing duplicate address detection, which has no
address to receive a unicast reply on — is answered on `ff02::1`.

## LLM failure → silence. This is deliberate.

NDP is in the **deliberately-silent** class the root `CLAUDE.md` catalogues, and its case is among
the strongest there. Every message the protocol defines is a *positive assertion* about addressing
on this link, and the peer writes it straight into its stack. There is no error message, no NAK
and no refusal.

So on an LLM failure this server transmits **nothing**, and no `WireFailure` text ever reaches the
wire. Fabricating a message to signal "netget is broken" would be cache poisoning at best — and,
if the message happened to be a Router Advertisement, a full traffic redirect. Not hearing from us
is something the peer's own retransmission already handles.

Because all outcomes look identical on the wire, they are separated in the log by a `decision=`
tag, the way `src/server/radius/` separates its cases:

| Tag | Meaning |
|---|---|
| `decision=no_policy` | No instruction and no handler — nothing sent, and **no LLM call at all** |
| `decision=model_reject` | The model answered `no_response` — a real decision |
| `decision=model_silent` | The model returned nothing usable |
| `decision=fail_closed_overloaded` | The call failed and `WireFailure::classify` says the backend is saturated (retryable) |
| `decision=fail_closed_llm_error` | The call failed otherwise |

The full error goes to `tracing::error!` and the status stream — both operator-facing, both local.
`tests/server/ndp/e2e_test.rs` asserts each of these, including that **no datagram at all** follows
a failure.

### Default behaviour: observe, no LLM

With no operator policy (no server instruction, no event handler) the server observes and says
nothing, **without** an LLM round-trip per received packet — `operator_wants_dynamic` in `mod.rs`,
the same gate `arp`, `lldp` and `ospf` use. A raw ICMPv6 socket on a busy link sees a great deal,
and there is no address we can honestly claim with no configured policy.

## Maturity, and what would earn Beta

`Experimental`, precisely:

- **Proven**: the codec, in both directions, against literal RFC 4861 / 4443 / 8200 / 8106 / 4291
  byte layouts — including every option's 8-octet length unit and the pseudo-header checksum. Plus
  the whole event → handler/LLM → action → message path over the UDP transport, in-process, with
  the emitted checksum verified against the addresses the message claims to have travelled between.
- **Not proven**: the raw ICMPv6 transport. No message this code produced has reached a real IPv6
  stack. No third-party NDP peer is runnable in the environment that tests it.

The bar for Beta, per the root `CLAUDE.md`, is *a real independent peer completing a real
exchange*. Here is the experiment that would do it, and it needs no hardware — **this machine has
the `feth` driver** (`sysctl net.link.fake.txstart` is `1`):

```bash
sudo ifconfig feth0 create
sudo ifconfig feth1 create
sudo ifconfig feth1 peer feth0        # a real Ethernet pair, entirely in software
sudo ifconfig feth0 up && sudo ifconfig feth1 up

# The peer is a real IPv6 stack: enable IPv6 on feth1 and let it autoconfigure.
sudo netget --server ndp --interface feth0 ...

# Direction 1 — our advertisement configures a real stack:
ifconfig feth1              # must show a 2001:db8:1::/64 address built by SLAAC
netstat -rn -f inet6        # must show our link-local as the default router
ndp -a                      # our link-layer address, against the address we claimed
scutil --dns                # the RDNSS servers we handed out

# Direction 2 — a real stack's messages reach us as events:
brew install ndisc6 && rdisc6 feth1       # a real Router Solicitation
ping6 -c1 fe80::1%feth1                   # a real Neighbour Solicitation
```

`radvd` on Linux would be the other half: point it at one end of a `veth` pair and require that
its advertisement arrives as a `ndp_router_advertisement_received` event with matching prefixes,
lifetimes and RDNSS.

Both directions must hold before the rating moves. **This has not been run.** It needs root, which
no agent here has. Do not claim it, and do not promote on the codec tests alone — that is the
mistake `wireguard` made, and the correction is recorded in the root `CLAUDE.md`.

## Not implemented

- **Duplicate Address Detection as a participant.** A solicitation from `::` is decoded and the
  default destination handles it correctly, but nothing defends an address of our own.
- **A neighbour cache.** By design: the root `CLAUDE.md` forbids a protocol implementing storage.
  Each message is an independent event; the model decides every answer. The 10-second
  connectionless sweep reaps bookkeeping entries, which is not the same thing.
- **Retransmission and the RFC 4861 state machines** — `RetransTimer`, `ReachableTimer`,
  INCOMPLETE/REACHABLE/STALE/DELAY/PROBE. The `retrans_timer` and `reachable_time` fields are
  *advertised* to hosts; nothing here obeys them.
- **Route Information (type 24, RFC 4191)** and **DNSSL (type 31, RFC 8106)**. Both decode as
  `NdpOption::Other` and neither can be sent. Route Information is the obvious next feature — it
  is how a rogue router injects a *specific* route rather than a default one.
- **Sending a Redirect.** Type 137 is decoded and raised as an event, but there is no
  `send_redirect` action: a Redirect is only legitimate from the router a host is already using,
  and getting the semantics wrong would be worse than not offering it. Add it deliberately if
  someone wants it.
- **SEND / RFC 3971 (Cryptographically Generated Addresses).** Nothing here is authenticated,
  which is the protocol's normal state and the reason it is interesting.
- **The Redirected Header option (type 4).** It carries as much of the redirected packet as fits,
  which is octets, which is exactly what must not cross the LLM boundary.
- **No storage of any kind.**

## Example prompts

```
Be an IPv6 router on en0: answer every router solicitation with 2001:db8:1::/64, RDNSS
2001:db8:1::53 and MTU 1500. Answer neighbour solicitations only for addresses inside that
prefix; use no_response for anything else.
```

```json
{"type": "open_server", "base_stack": "ndp", "interface": "en0",
 "event_handlers": [{"event_pattern": "ndp_router_solicitation", "handler": {"type": "static",
   "actions": [{"type": "send_router_advertisement", "router_lifetime": 1800,
     "prefixes": [{"prefix": "2001:db8:1::", "length": 64}],
     "rdnss": ["2001:db8:1::53"], "mtu": 1500}]}}]}
```

```
NDP observer on en0: log every router advertisement you see — who sent it, which prefixes and
which DNS servers — and send nothing at all.
```

## Security note

Neighbour Discovery is unauthenticated by design, and a peer will believe it. This server can
therefore claim any IPv6 address on a link, and can reconfigure every host on it — address, route
and DNS — with a single message. That is not a weakness in this implementation; it is what the
protocol is, and it is why `mitm6` is a two-minute attack.

Use it on links you own or have permission to test. The same property is what makes it useful as a
honeypot, as a lab router, and for teaching how much an IPv6 host will accept from a stranger.
