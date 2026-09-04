# LLDP Protocol Implementation

LLDP (IEEE 802.1AB) link-layer discovery: every agent periodically announces a description of
itself to the nearest-bridge group address, and every agent writes what it hears into a table an
operator reads. There is no request, no response and no acknowledgement.

**State**: `Experimental`, and the reason is specific — see [Maturity](#maturity-and-what-would-earn-beta).
**Privilege**: `PrivilegeRequirement::RawSockets`.
**Connectionless**: declared, so the 10-second idle sweep reaps the per-neighbour entries.
**Stack**: `ETH>LLDP`. EtherType `0x88CC`, destination `01:80:C2:00:00:0E`.
**Spec**: [IEEE 802.1AB-2016](https://standards.ieee.org/standard/802_1AB-2016.html).

## What makes this protocol worth driving from a model

The identity is **authored, not derived**. Nothing in a received advertisement determines what
we should advertise back — there is no query to answer. Chassis ID, port ID, system description
and capability set are choices, and they are exactly the fields network reconnaissance reads off
a link. NetGet impersonates whatever device the model describes.

That is also why the whole design refuses byte-level parameters. A `tlv_hex` field would be
unreadable to a model and would leave nothing for it to decide.

## Files

| File | What it is |
|---|---|
| `codec.rs` | The **pure** TLV codec. No socket, no config, no global state. This is the part that is proven. |
| `actions.rs` | `Protocol` + `Server`: two actions, two events, metadata, startup parameters. |
| `mod.rs` | Transport. Two of them, both thin over `codec.rs`. |

### The split is the point

The raw transport needs `CAP_NET_RAW` / `/dev/bpf*`, which nothing in this repository has, so
the transport can never be executed here. The frame format can be, and is — against literal
specification bytes in `tests/server/lldp/codec_test.rs`. This is the `bluetooth_ble_beacon`
precedent the root `CLAUDE.md` describes: *"payload construction is pure and exhaustively
unit-tested against literal spec bytes; its BlueZ transport has never been compiled or run"*.
`metadata().notes` says which half is which, which is what `Experimental` is for.

Keep `codec.rs` pure. The moment it reads a socket or a config value, the only testable part of
this protocol stops being testable.

## Transports

### `transport: "raw"` (default)

libpcap capture and injection on a real interface, filtered to `ether proto 0x88cc`.

**Never executed anywhere.** It compiles, and it is written to the same shape as `arp` and
`isis`, including the parts those learned the hard way:

- **Startup reports failure.** The capture is opened inside `spawn_blocking`, but the outcome
  comes back over a `oneshot` and `spawn()` only returns `Ok` once the handle is genuinely open.
  ARP, DataLink and ICMP each shipped the fire-and-forget version and were each fixed
  separately; IS-IS was missed all three times. A server in `Running` that has captured nothing
  is worse than one that refuses to start.
- **The BPF filter failure is fatal too.** The expression is fixed, so a failure to compile it
  means the capture would deliver *every* frame on the segment to the model.
- **It cannot work on loopback, and says so.** `ether proto` is an Ethernet-only keyword; on
  `lo`/`lo0`, tunnels and raw-IP devices libpcap compiles it to "expression rejects all packets"
  and errors. The message names the interface and the link layer rather than quoting libpcap's
  optimiser.
- **Stopping is cooperative.** `JoinHandle::abort()` cannot interrupt a thread parked in
  `next_packet()`, so the loop polls a `StopSignal` and the registered task trips it.
- **Our own frames are ignored on receive.** An injected advertisement captured back off the
  wire would raise an event, which would produce another advertisement, forever. Frames whose
  source MAC equals ours are dropped before the event is built.

### `transport: "udp"` (testing)

Complete Ethernet frames carried as UDP datagram payloads. This is the `ospf` accommodation
(`Cargo.toml`: *"requires root/CAP_NET_RAW, tests use UDP"*) made **explicit as a declared
startup parameter** rather than left to the test file — `ospf`'s E2E suite starts a plain UDP
server and hand-rolls packets, so it never touches `ospf`'s own code at all. Here the datagram
carries the real frame, so the real codec, the real event, the real action executor and the real
frame builder all run.

No real neighbour speaks it, and the parameter description says so twice.

**One consequence worth knowing**: because `metadata()` declares `RawSockets`, `server_startup`
refuses to start LLDP at all on an unprivileged host — *including* in UDP mode, since privilege
is a static property of the protocol and cannot depend on a parameter. So the UDP transport is
reachable by calling `Server::spawn` directly (which is what the test suite does) and not
through `open_server`/MCP without privilege. Declaring `None` to work around that would be a lie
about the transport anyone actually uses.

## Startup parameters

Four declared, four read, all in `LldpServer::spawn_with_llm_actions`.

| Parameter | Default | What reads it |
|---|---|---|
| `transport` | `"raw"` | Selects `spawn_raw` or `spawn_udp` |
| `udp_peer` | last peer heard from | Destination for outgoing datagrams. **Rejected with `transport: "raw"`**, where it would do nothing |
| `advertise_interval_secs` | `0` (disabled) | Period of the `lldp_advertise_due` timer |
| `source_mac` | the interface's own address, else `02:00:00:00:00:01` | Ethernet source of transmitted frames, when the action names none |

`source_mac`'s fallback uses `pnet::datalink::interfaces()`, which works on every platform pnet
supports — deliberately not `/sys/class/net/<if>/address`, which is why `isis` sends from a
placeholder everywhere except Linux.

## What the model sees and controls

### Events

| Event | When |
|---|---|
| `lldp_neighbor_advertisement` | A neighbour advertised itself; every TLV is already decoded |
| `lldp_advertise_due` | The advertise timer fired — announce ourselves, unprompted |

Both declare the full action set with `.with_actions(...)`, and both have real emit sites
(`handle_frame` and `spawn_advertise_timer`). An event declared and never raised is a defect
this repository has shipped in bulk; `tests/event_emit_sites_test.rs` guards it.

Event data is names and natural notation throughout — `"mac_address"`, `"00:01:30:f9:ad:a0"`,
`["bridge", "router"]` — plus `*_subtype_code` numbers for anyone who wants them. Optional TLVs
the neighbour did not send are **absent**, not `null`, so a script can test with a plain `in`.

### Actions

| Action | Effect |
|---|---|
| `send_lldp_advertisement` | Builds and transmits one LLDPDU from structured fields |
| `no_advertisement` | Say nothing, deliberately. Logged `decision=model_reject` |

`send_lldp_advertisement` is validated **at the action** by running the encoder: a chassis ID
that does not match its subtype, a capability name with a typo, a description past the
255-octet TLV limit. The model is told, rather than a neighbour silently discarding the frame.

Two defaults worth knowing, both chosen because the alternative would misrepresent something:

- **An omitted subtype is inferred from the value's shape**, not defaulted to a constant. A MAC
  becomes `mac_address`, an IP becomes `network_address`, anything else becomes `local`. Sending
  a MAC under `interface_name` would put the literal text `"00:1b:21:…"` in a neighbour's table.
- **Capabilities given once count as both supported and enabled.** `enabled = 0` would describe
  a device that supports being a bridge and is not one, which is not what "advertise as a
  switch" means.

## LLM failure → silence. This is deliberate.

LLDP is in the **deliberately-silent** class the root `CLAUDE.md` catalogues, and it is one of
the clearer cases. Every frame the protocol defines is a *positive assertion* that a device with
a given identity exists on this link, and a neighbour writes it straight into its topology table
and shows it to an operator. There is no error frame, no NAK and no refusal message.

So on an LLM failure this server transmits **nothing**, and no `WireFailure` text ever reaches
the wire. Fabricating an advertisement to signal "netget is broken" would put a device that does
not exist on somebody's network map — strictly worse than the neighbour simply not hearing from
us, which its own TTL already handles.

Because all outcomes look identical on the wire, they are separated in the log by a `decision=`
tag, the way `src/server/radius/` separates its cases:

| Tag | Meaning |
|---|---|
| `decision=no_policy` | No instruction and no handler — nothing advertised, and **no LLM call at all** |
| `decision=model_reject` | The model answered `no_advertisement` — a real decision |
| `decision=model_silent` | The model returned nothing usable |
| `decision=fail_closed_overloaded` | The call failed and `WireFailure::classify` says the backend is saturated (retryable) |
| `decision=fail_closed_llm_error` | The call failed otherwise |

The full error goes to `tracing::error!` and the status stream — both operator-facing, both
local. `tests/server/lldp/e2e_test.rs` asserts each of these, including that **no datagram at
all** follows a failure.

### Default behaviour: listen, no LLM

With no operator policy (no server instruction, no event handler) the server observes and says
nothing, **without** an LLM round-trip per captured frame — `operator_wants_dynamic` in `mod.rs`,
the same gate `arp` and `ospf` use. What to advertise is policy, and with no configured identity
there is nothing honest to claim.

## Maturity, and what would earn Beta

`Experimental`, precisely:

- **Proven**: the TLV codec, in both directions, against literal 802.1AB-2016 byte layouts and
  against the mandatory/capability/management TLVs of a real-world capture. Plus the whole
  event → handler/LLM → action → frame path, over the UDP transport, in-process.
- **Not proven**: the raw-Ethernet transport. No frame this code produced has reached a real
  LLDP neighbour. No third-party LLDP peer is runnable in the environment that tests it.

The bar for Beta, per the root `CLAUDE.md`, is *a real independent peer completing a real
exchange*. Here is the experiment that would do it, and it needs no hardware — **this machine
has the `feth` driver** (`sysctl net.link.fake.txstart` is `1`):

```bash
sudo ifconfig feth0 create
sudo ifconfig feth1 create
sudo ifconfig feth1 peer feth0        # a real Ethernet pair, entirely in software
sudo ifconfig feth0 up && sudo ifconfig feth1 up

brew install lldpd
sudo lldpd -d -I feth1                # the real peer, on the other end of the pair

sudo netget --server lldp --interface feth0 ...
lldpcli show neighbors details        # must show the identity the model authored
```

Two directions have to hold before the rating moves: `lldpcli` shows our chassis/port/system
fields exactly as the model wrote them, and `lldpd`'s own advertisements arrive as a
`lldp_neighbor_advertisement` event with matching fields.

**This has not been run.** It needs root, which no agent here has. Do not claim it, and do not
promote on the codec tests alone — that is the mistake `wireguard` made, and the correction is
recorded in the root `CLAUDE.md`.

## Not implemented

- **Organisationally-specific TLVs (type 127)**: decoded frames skip them; nothing can send
  them. That covers LLDP-MED (the ones an IP phone cares about), 802.1 VLAN TLVs, 802.3 MAC/PHY
  and Power-over-Ethernet. A real switch sends several in every frame, and this server sends
  none. This is the largest gap and the obvious next feature.
- **Multiple management addresses.** 802.1AB permits several; one is encoded and only the first
  received is surfaced.
- **The management address OID.** Decoded far enough to validate the TLV's length, then
  discarded — it is an SNMP OID, of no use to a model, and nothing here could produce one.
- **A neighbour table with TTL ageing.** Each advertisement is an independent event; nothing is
  stored between them. The 10-second connectionless sweep reaps the bookkeeping entries, which
  is not the same thing as honouring a neighbour's TTL.
- **The shutdown frame is expressible but not automatic.** `ttl: 0` is what tells a neighbour to
  delete us; nothing sends one when the server stops.
- **802.1AB transmit-state-machine timing** — `txDelay`, `txFastInit` and the rest. The timer is
  a plain interval.
- **No storage of any kind.** The model invents every neighbour.

## Example prompts

```
Be an LLDP agent on en0 impersonating a Cisco Catalyst 2960: answer every neighbour
advertisement with chassis 00:1b:21:3c:4d:5e on GigabitEthernet0/1, capabilities bridge and
router, TTL 120.
```

```json
{"type": "open_server", "base_stack": "lldp", "interface": "en0",
 "startup_params": {"advertise_interval_secs": 30},
 "event_handlers": [{"event_pattern": "lldp_*", "handler": {"type": "static",
   "actions": [{"type": "send_lldp_advertisement",
     "chassis_id": "02:00:00:00:00:01", "chassis_id_subtype": "mac_address",
     "port_id": "1/1", "port_id_subtype": "interface_name", "ttl": 120,
     "system_name": "netget-lab", "capabilities": ["bridge", "router"]}]}}]}
```

```
LLDP honeypot on eth0: log every neighbour you see and advertise nothing
(no_advertisement for everything).
```

## Security note

An LLDP advertisement is unauthenticated by design, and a neighbour will believe it. This server
can therefore put an arbitrary device onto somebody's network map — a fake switch, a fake IP
phone, a fake router with a management address that is not ours. Use it on links you own or have
permission to test. The same property is what makes it useful as a honeypot and for teaching how
much a switch leaks to anyone on the segment.
