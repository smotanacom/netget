# CDP Protocol Implementation

Cisco Discovery Protocol server. A switch shouts its hostname, hardware model,
IOS version, port name and **native VLAN** onto the wire every 60 seconds,
unsolicited, to anyone listening. Here the *model* authors all of it, so what a
neighbour records about "the switch it is plugged into" is whatever the LLM
decides to claim.

**State**: `Experimental`.
**Privilege**: `PacketCapture`.
**Stack**: `ETH(802.3)>LLC/SNAP>CDP`. **Connectionless**: yes.

## Read this first: what is proven and what is not

The split is the whole point of the module layout, and the maturity rating rests
on it.

| Part | Evidence |
|---|---|
| **`codec.rs`** — TLVs, 802.3 + LLC/SNAP framing, the checksum | Literal specification bytes **and** three real captured CDP packets decoded by an independent implementation. See "Checksum" and "What the codec is proven against". |
| **Event → LLM → action → frame** | Exercised end to end in `tests/server/cdp/e2e_test.rs` over the declared UDP test transport, including that an LLM failure emits nothing. |
| **The raw 802.3 transport** | **Never executed.** Not by a test, not by hand. See "The honest gap". |

`Experimental` is the rating that fits: the code compiles and has real tests, and
no real Cisco device or third-party CDP peer has ever seen a frame from it. Do
not read it as "works on a real switch".

## Why the codec is a separate, pure module

Opening a pcap handle needs packet-capture privilege, which the test environment
does not have. If the wire format lived inside the transport it would be
untestable, and the protocol's only evidence would be "it compiles" — which is
how `wireguard` ended up rated `Stable` on a test that mocked events and actions
that did not exist (root `CLAUDE.md`).

So `codec.rs` performs **no I/O of any kind**: no socket, no capture handle, no
`AppState`, no LLM. It is a total function between bytes and structured values,
and every claim this protocol makes about the wire is tested there. `mod.rs` is a
thin transport over it. This is the `bluetooth_ble_beacon` precedent: pure
payload construction exhaustively tested against literal spec bytes, transport
never executed, and `metadata().notes` saying both.

## Wire format

```text
destination MAC (6)  01:00:0C:CC:CC:CC   -- always; there is no unicast CDP
source MAC      (6)
802.3 length    (2)  = 8 + len(CDP payload)   -- a LENGTH, not an EtherType
LLC  DSAP       (1)  0xAA
LLC  SSAP       (1)  0xAA
LLC  control    (1)  0x03  (unnumbered information)
SNAP OUI        (3)  00:00:0C  (Cisco)
SNAP protocol   (2)  0x2000    (CDP)
CDP  version    (1)  1 or 2
CDP  TTL        (1)  seconds the neighbour keeps the entry
CDP  checksum   (2)
CDP  TLVs       (n)  type(2) length(2) value(length-4)
```

**A TLV's `length` includes its own 4-byte header.** This is the single most
common mistake when hand-writing CDP: a Device ID of `"myswitch"` (8 bytes) is
`00 01 00 0C`, not `00 01 00 08`.

TLVs modelled: Device ID (0x0001), Addresses (0x0002), Port ID (0x0003),
Capabilities (0x0004), Software Version (0x0005), Platform (0x0006), Native VLAN
(0x000A), Duplex (0x000B), Management Address (0x0016). They are emitted in
ascending type order, which is what real devices do and what makes the output
diffable against a capture.

Anything else — Protocol Hello, VTP domain, Trust Bitmap, the Power TLVs — is
parsed as far as its type and length and reported to the model as
`{"type": 8, "name": "protocol_hello", "length": 36}`. **The bytes are dropped
deliberately**: no event in this project carries raw bytes or base64 to the
model, and a length is the honest amount of information available without
modelling the TLV.

### Addresses

`count(4)`, then per entry `protocol_type(1) protocol_length(1) protocol(var)
address_length(2) address(var)`. IPv4 is NLPID `01 01 CC`, length 4; IPv6 is
802.2 SNAP `02 08 AA AA 03 00 00 00 86 DD`, length 16. Any other family (CLNS,
DECnet, AppleTalk) is skipped on decode rather than guessed at.

## Checksum — the field everyone gets wrong

It is a 16-bit one's-complement sum (RFC 1071) over the CDP payload with the
checksum field zeroed, **with Cisco's non-standard treatment of an odd-length
payload.** For an even-length payload it is the plain IP checksum. For an odd
one, RFC 1071 says pad with a trailing zero byte, making the final word
`last << 8`. Cisco instead puts the last octet in the *low* half of the final
big-endian word and then compensates for an off-by-one in its own sign handling:

| last byte `L` | final 16-bit word |
|---|---|
| `L < 0x80`  | `0x0000 \| L` |
| `L >= 0x80` | `0xFF00 \| (L - 1)` |

This is not a guess. It is transcribed from Wireshark's `packet-cdp.c`, which
builds exactly that padded buffer ("Swap bytes in last word" / "Compensate
off-by-one error") before calling `in_cksum`, and it is corroborated by scapy's
independent `_CDPChecksum._check_len`.

**The two disagree on exactly one value.** scapy tests `last <= 0x80` where
Wireshark tests `last & 0x80`, so for a payload ending in `0x80` scapy produces
`0x0080` and Wireshark `0xFF7F`. This codec follows **Wireshark**, because
Wireshark's reading is the one that treats the octet as signed (`0x80` is
negative, so it takes the compensated branch) and because Wireshark is what an
operator will check our frames with. `codec_test.rs` pins the boundary value so
the choice is a decision rather than an accident.

Getting this wrong is not cosmetic: Wireshark flags the packet `[incorrect]` and
a real Cisco device discards it — the same failure the OSPF Fletcher-vs-IP
checksum bug had (`src/server/ospf/actions.rs`).

Every odd-length assertion in the test file **also** asserts that the RFC 1071
answer is different, so "simplifying" the padding away cannot pass.

## What the model sees and controls

**Event**: `cdp_neighbor_advertisement`, one per received frame. Carries
`source_mac`, `destination_mac`, `version`, `ttl`, `device_id`, `port_id`,
`platform`, `software_version`, `capabilities` (named flags) and
`capabilities_value`, `native_vlan`, `duplex`, `addresses`,
`management_addresses`, `checksum_valid`, and `other_tlvs`. All structured; no
bytes anywhere.

**Actions**

| Action | Effect |
|---|---|
| `send_cdp_advertisement` | builds and emits one CDP frame. `device_id` required; everything else optional |
| `no_advertisement` | explicit refusal. Emits nothing; logged `decision=model_reject` |

No async actions. CDP is one-way and unsolicited — there is nothing to ask a CDP
*server* to do out-of-band that is not "advertise", and advertising is what the
sync action already does. Declaring an async duplicate would advertise a verb
with no state behind it.

Capabilities are **named**, not numeric: `["switch", "igmp"]` is something a
language model produces correctly and `0x28` is not. A numeric bitmask is still
accepted. An unrecognised name is **rejected**, not ignored — silently dropping
one would emit an advertisement claiming less than the operator asked for with
nothing saying so.

## LLM failure → silence. Always.

CDP is in the **deliberately-silent** class (root `CLAUDE.md`). Every frame the
protocol defines is a positive assertion — *a device with this identity exists on
this link* — which the neighbour caches for `ttl` seconds and an operator reads
back from `show cdp neighbors detail`. There is no CDP error frame, no NAK, no
way to say "ask again later".

So a fabricated advertisement on failure would turn "netget's backend is down"
into "this switch is real", written into someone's neighbour table. That is
strictly worse than silence, and silence costs the peer nothing: it simply ages
us out. **Nothing derived from the error ever reaches the wire** — no
`WireFailure` text, no backend URL, no model name.

The distinction survives only in the log, in the shape `src/server/radius/` uses:

| Situation | Log tag | Wire |
|---|---|---|
| LLM call failed, backend saturated | `decision=fail_closed_llm_error_overloaded` | nothing |
| LLM call failed, backend broken | `decision=fail_closed_llm_error_unavailable` | nothing |
| Model chose `no_advertisement` | `decision=model_reject` (+ its reason) | nothing |
| Model answered with no action at all | `decision=model_silent` | nothing |
| Model's action was malformed | `decision=model_invalid_action` | nothing |
| No operator policy configured | `decision=passive_no_policy` | nothing, **and no LLM call** |

A malformed answer is a failure to answer, not a licence to invent a device.

## Passive by default

With no server instruction and no event handler for
`cdp_neighbor_advertisement`, the server observes and never consults the model.
Two reasons: whether to announce ourselves to a neighbour at all is a policy
decision the received frame cannot make, and a busy segment produces one
advertisement per neighbour per 60s, so consulting the model with no policy would
burn a round-trip per frame to decide nothing. Same gate as `isis` and `ospf`.

## Transports

Selected by the `transport` startup parameter.

### `raw` (default)

libpcap capture and injection on the bound interface. The BPF filter is
destination `01:00:0c:cc:cc:cc` **and** the full SNAP header **and** protocol
`0x2000` — without a filter the capture hands *every* frame on the segment to the
LLM, so a filter that fails to compile refuses the start rather than falling
through (the reasoning `src/server/arp/mod.rs` records).

Opening the handle is the privileged step, so it is **not** fire-and-forget: it
happens on a blocking thread whose outcome comes back over a oneshot, and
`spawn()` only returns `Ok` once the capture is genuinely live. A failure
surfaces as `ServerStatus::Error`, not as a server reporting `Running` while
capturing nothing — the defect root `CLAUDE.md` records for ARP, DataLink and
ICMP.

`JoinHandle::abort()` cannot interrupt a thread parked in `next_packet()`, so the
loop stops cooperatively through `crate::utils::StopSignal`, whose park task is
what gets registered with `register_server_task`.

The interface MAC comes from `pnet::datalink::interfaces()` — cross-platform,
unlike `isis`'s `/sys/class/net` read, which returns an error on macOS.

### `udp` (test transport)

Binds a UDP socket on the bound host/port; each datagram is one complete 802.3
CDP frame, and replies go back to the datagram's sender. Needs no privilege.

This is **not** a wire-compatible CDP transport and does not pretend to be — real
CDP has no UDP encapsulation. It exists because everything above the framing is
transport-independent and worth testing. `ospf` set the precedent of driving a
privileged protocol over UDP in tests; making it a **declared mode** rather than
a test that quietly runs a different protocol is the improvement.

## Privilege: `PacketCapture`, not `RawSockets`

CDP is captured and injected at the link layer through libpcap. It never opens a
`SOCK_RAW`. `arp`, `datalink` and `isis` — the three existing pcap-based L2
protocols — all declare `PacketCapture`, and so does this.

Declaring `RawSockets` would claim more than the protocol needs and refuse to
start on a machine that has `/dev/bpf*` access but is not root, which is a real
and common configuration (macOS with ChmodBPF; Linux with `CAP_NET_RAW` only).
That is the mistake root `CLAUDE.md` records for `ospf` declaring `Root` when it
wanted `CAP_NET_RAW`.

**One consequence to know before running the tests.** The privilege gate lives in
`server_startup.rs` and is checked against `metadata()`, which cannot vary by
transport — so starting a CDP server *at all* requires packet-capture capability,
even under the `udp` transport that uses none. On a machine with no `/dev/bpf*`
access the e2e tests fail at startup with a privilege message. That is a
limitation of a metadata-level gate, not a defect here, and the test file says so
loudly rather than skipping.

## Startup parameters

Both are read in `CdpServer::spawn_with_llm_actions`; the whole-tree
`startup_param_drift_test` fails the build for a declared parameter nothing
reads.

| Parameter | Read for |
|---|---|
| `transport` | `"raw"` (default) or `"udp"` |
| `source_mac` | source MAC of frames we emit. Overrides the interface MAC on `raw`; the only way to set one on `udp`. Defaults to the interface MAC (raw) or `02:00:0c:cc:cc:01` (udp) |

The flexible-binding `mac_address` field means the same thing as `source_mac` and
is honoured; an explicit `source_mac` wins because it is the more specific
request.

## Text TLVs are screened, and asymmetrically

A CDP text TLV is an entry in somebody's device table, copied verbatim into
`show cdp neighbors detail` and into this protocol's own
`CDP advertisement from {device_id} ({platform}) on {port_id}` log line — which
`src/protocol/log_template.rs` renders with no quoting. A newline in `device_id` forges a whole
extra neighbour entry in both. The TLV is length-prefixed, so such a frame is *legal CDP*; only
the rendering is the problem, which is why `codec.rs` is where it is handled.

Encode **refuses** a control character in `device_id` / `port_id` / `platform` and names the
field; decode **replaces it with a space**. The asymmetry is deliberate: on the encode side the
model wrote the string and can be told, while a neighbour cannot be asked to resend and dropping
its advertisement would hide a device that is really there.

**Software Version is exempt in both directions, deliberately.** A real IOS banner is
multi-line — the scapy Catalyst capture in the tests contains several `0x0a` bytes — it is the
most useful single thing a recon operator reads off a CDP frame, and it appears in no log
template. The exemption has its own test, so a later "simplification" that refuses everything
cannot pass either.

Separately, the 802.3 length field is bounded at 1500 in `encode_frame`. It is not an MTU
preference: IEEE 802.3 reserves `0x0600` (1536) and above for EtherType, so a longer frame does
not become oversized, it stops being an 802.3 frame — every receiver reads it as Ethernet II and
the CDP behind it is never parsed. The model's text fields were unbounded, so it could produce
one. `MAX_TEXT_TLV` (255) bounds each field individually as well, purely so the error names the
offending field rather than complaining about the whole frame.

## Not implemented

- **No periodic advertisement timer.** Real CDP announces every 60s
  unprompted; this server only answers a neighbour it has heard. Use
  `scheduled_tasks` if you want a heartbeat.
- **No neighbour table.** Protocols do not implement storage; the model keeps
  whatever it wants in server memory.
- **No CDPv1 quirks.** Version 1 is accepted and can be emitted, but the v1/v2
  TLV differences are not modelled.
- **No VTP, Power-over-Ethernet, Trust Bitmap or IP Prefix TLVs** on the emit
  side — they are only reported on receive.
- **No 60-byte Ethernet padding** on emit. A NIC pads on transmit, and the UDP
  transport has no minimum; padding here would make the emitted bytes differ from
  what the codec tests assert.
- **No TTL-expiry tracking.** We do not age neighbours out.

## The honest gap, and the concrete path to Beta

The raw 802.3 transport has never been executed. Every byte it would put on the
wire is tested; the pcap read and write calls around them are not.

This machine has the **`feth` driver** (`net.link.fake.txstart: 1`), so a real
Ethernet pair exists with no hardware:

```bash
sudo ifconfig feth0 create
sudo ifconfig feth1 peer feth0
```

Point a CDP server at `feth0` with `transport: "raw"`, send a captured
advertisement in on `feth1`, and check the reply with Wireshark's own CDP
dissector — which is the independent decoder that would make the checksum claim
above evidence about *our emitted frames* rather than about our parser. Better
still, plug into a real Cisco switch and read `show cdp neighbors detail`.

**Neither has been done, and neither may be claimed until it is.** Root
`CLAUDE.md` records three protocols demoted for treating a codec or a skipped
test as evidence of a real peer; a capture decoded by scapy is evidence about the
codec and nothing more.
