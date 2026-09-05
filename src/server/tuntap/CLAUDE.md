# TUN/TAP interface endpoint

NetGet becomes one end of a real network interface. The host routes packets to it and every
one of them is decoded into structured header fields. This is not another protocol on a
socket — it is the layer *below* every protocol, so any layer-3 service can be synthesised
without implementing it.

That is the power. The trap is in the same sentence.

---

## The design problem, and the answer

**A per-packet LLM call is hopelessly slow.** One `ping` is one packet per second. A TCP
handshake is three packets in milliseconds. Anything real is thousands of packets. A version
that asks the model per packet looks broken and burns the whole LLM budget in seconds.

So the model is not on the default path at all. Three gates run in order, and the model is the
last one:

```
  frame from the interface
        │
        ├─ decode (native code)                    ── undecodable ─▶ counted, dropped
        │
        ├─ GATE 1  packet_filter (native code)     ── no match ────▶ counted, dropped
        │          default "icmp"
        │
        ├─ GATE 2  the server's event_handlers     ── a rule matches ▶ script/static/manual
        │          THE PRIMARY ANSWER PATH                            answers; no model call
        │
        └─ GATE 3  llm_escalation + a rolling      ── over budget ──▶ counted, dropped,
                   per-minute budget, default 6                       decision=fail_closed_rate_limited
                         │
                         ▼
                   one model call
```

### Gate 1 — `packet_filter`

Native code, on every packet, before anything is allocated. This is where the volume goes, and
it is the parameter an operator actually tunes.

Grammar (`src/server/tuntap/packet.rs`, `PacketFilter::parse`):

* `all` — everything. `none` — nothing (the interface still runs and still counts).
* otherwise comma-separated alternatives (OR) of `+`-separated terms (AND):
  `v4`, `v6`, a protocol name (`icmp`, `icmpv6`, `tcp`, `udp`, `gre`, `esp`, `ospf`, …),
  `ip-proto-<n>`, `tcp:<port>`, `udp:<port>`, `port:<n>`, `from:<addr>`, `to:<addr>`,
  `host:<addr>`, and `tcp-syn`.

**`tcp-syn` is the most useful term in the language** and the reason the filter is expressive
rather than a boolean. It matches a TCP segment with SYN set and ACK clear, so a whole TCP
connection costs *one* event instead of one per packet. `"tcp-syn+to:10.7.0.2, icmp"` reads as
"a new connection to that address, or any ping".

The default is `"icmp"`. That is deliberate and stated in the parameter's own description: a
ping is one packet a second and is the one thing an operator can watch by hand. It is not a
guess at what is interesting — it is the smallest thing that visibly works.

### Gate 2 — `event_handlers` are the answer path

The root `CLAUDE.md` says scripts and static handlers are "the right default for deterministic
behavior… Reserve the LLM for responses that genuinely require reasoning." Here that is not a
preference, it is the only way the protocol works at volume. `get_startup_examples()`'s script
and static modes both answer pings entirely in-process, and the static one shows the shape that
matters: `{{event.icmp_id}}` / `{{event.icmp_sequence}}` interpolation, so the correlation the
sender matches on survives without anyone reasoning about it.

A matched script, static or manual rule is **exempt from the budget**, because it costs no
model call. An explicit `{"type":"llm"}` rule is **not** exempt: asking for the model by name
does not raise the ceiling.

### Gate 3 — `llm_escalation` + `llm_max_per_minute`

`llm_escalation: "never"` makes the server purely deterministic — handlers answer, everything
else is dropped, no LLM budget is ever spent. `"unhandled"` (default) lets a packet nothing
claimed reach the model, at most `llm_max_per_minute` times per **rolling** minute (default 6).

A sliding window, not a leaky bucket, because the guarantee the parameter promises is the
literal one: never more than N in any minute. A bucket refilling continuously permits bursts
above N, which is the thing being prevented.

Over-budget packets are **dropped, not queued.** A queued packet is meaningless once the sender
has retransmitted or given up, and a queue is how a busy interface turns into unbounded memory.

At the defaults, a ping arriving once a second produces six model calls in the first minute and
then silence. That is intentional and says so in the parameter description: the *filter* is how
you decide what is interesting; the budget is a ceiling, not a scheduler.

---

## Failure is silence, and the log carries the distinction

There is no error packet, and there is no `WireFailure` anywhere in this protocol. An LLM
failure, a refused `send_packet`, an exhausted budget, a full egress queue — every one of them
drops the packet and writes nothing.

**A fabricated packet on a real interface is indistinguishable from a spoof**, and NetGet has
no idea what the host expected. There is no reply that is safely wrong here: an invented TCP
RST tears down a real connection, an invented ICMP unreachable poisons a real path MTU.

Following `src/server/radius/`, the log carries what the wire cannot. `Decision::as_str()`
tokens, greppable and stable:

| token | meaning |
|---|---|
| `handler_send` / `handler_drop` / `handler_silent` | a deterministic rule answered, dropped, or produced nothing |
| `model_send` / `model_drop` / `model_silent` | the model answered, said `drop_packet`, or produced no usable action |
| `fail_closed_llm_error` | the LLM call failed |
| `fail_closed_build_error` | the answer named a packet that could not be built, or the wrong layer |
| `fail_closed_rate_limited` | the per-minute budget was exhausted |
| `llm_disabled` | `llm_escalation: "never"` and no rule claimed the packet |

`model_drop` and `model_silent` are separate tokens on purpose. Collapsing "it said no" into
"it said nothing" is the OAuth2 defect the root `CLAUDE.md` calls the most dangerous pattern in
this codebase. The `fail_closed_*` decisions log at ERROR; the rest at DEBUG, because an
interface produces far too many for INFO to stay readable.

---

## The 4-byte header, explicitly

macOS `utun` prepends four bytes to every packet in both directions: the address family as a
big-endian `u32` (`AF_INET` = 2, `AF_INET6` = 30). Linux prepends nothing when the device was
opened with `IFF_NO_PI`, and two bytes of flags plus a big-endian EtherType (`0x0800` /
`0x86DD`) when it was not.

This is the single most common source of "why is every packet off by four bytes", and it does
**not** fail loudly: the IP version nibble lands in the middle of the prefix, so the packet
decodes as garbage rather than erroring, and the symptom is "every field is wrong" rather than
"the header is misaligned".

So it is a named, tested type — `PacketInformation::{None, MacOsUtun, LinuxTunPi}` — and the
prefix is **validated, not skipped**: a header that does not name IPv4 or IPv6 is an error,
because it means the configured mode does not match the device.

The `packet_information` startup parameter defaults to `auto`, which resolves to `None`. That
is not an assumption: the `tun` crate normalises the platform difference itself
(`posix::Tun::new` sets a read/write `offset` of 4 whenever packet information is enabled), so
by the time a frame reaches NetGet it is already a bare packet. The other values exist for a
raw fd handed in from outside that machinery, and for the tests that pin the layout against
literal bytes.

---

## TUN vs TAP, and macOS

`mode` selects layer 3 (TUN, bare IP packets) or layer 2 (TAP, Ethernet frames). In TAP mode
the event carries an `ethernet` object and `send_packet` requires both `source_mac` and
`destination_mac`.

**macOS `utun` is TUN-only.** There is no TAP device without a third-party kext. `spawn()`
returns a clear `Err` naming the reason rather than quietly handing back a TUN interface —
the `bluetooth_ble_beacon` precedent from the root `CLAUDE.md`: refusing to start is not the
same as pretending, and it is not the same as hiding the protocol either.

`build_packet` is deliberately mode-agnostic (it emits an Ethernet frame when the answer named
both MACs, a bare IP packet otherwise) so its declared examples are executable anywhere. The
engine's `prepare_for_link` is what checks the answer against the interface's actual layer, and
a mismatch is **refused**, not corrected: silently stripping or inventing a link header would
put a packet on the wire that nobody described.

---

## No raw bytes across the boundary

Events carry decoded header fields — `ip_version`, `source`, `destination`, `protocol` (name
*and* number), `ttl`/`hop_limit`, `total_length`, ports, TCP flags as names, ICMP type as a
name, echo id and sequence, and a one-line `summary`.

A payload is genuinely opaque, so it appears as at most 64 bytes of `payload_preview` beside
the true `payload_length`, with `payload_encoding` stating `"utf8"` or `"hex"`. **NetGet decides
once and states the answer**; that is the producer declaring what it did, which is the opposite
of an executor sniffing what it was given. The preview is cut with
`crate::utils::truncate_for_log`, never by byte-index slicing.

In the other direction `send_packet`'s `payload_encoding` is genuinely decoded by the executor,
and hex is decoded strictly. `"48656c6c6f"` is simultaneously valid text and valid hex and only
the sender knows which it meant — the exact `send_tcp_data` bug the root `CLAUDE.md` records.
Invalid hex is a refusal, never a fallback to text.

---

## Structure

| file | what |
|---|---|
| `packet.rs` | **No I/O whatsoever.** Decode, build, checksums, the filter, `PacketInformation`. |
| `mod.rs` | `TunTapEngine` (the pipeline) and `TunTapServer` (the device). |
| `actions.rs` | Events, actions, metadata, the nine startup parameters. |

The split is the whole testing strategy: creating an interface needs root, so the transport can
never run in the suite, while the pipeline runs over a pair of `mpsc` channels
(`TunTapEngine::spawn_over_channels`) — *the same `run()` the real device calls*, with the file
descriptor replaced.

`spawn()` awaits readiness through a `oneshot` and returns `Err` when the device cannot be
created, so `server_startup` sets `ServerStatus::Error`. A server sitting in `Running` with no
interface is exactly the ARP/DataLink defect the root `CLAUDE.md` records, and this protocol
would hit it on every unprivileged start.

---

## Maturity: `Experimental`, and precisely why

**Proven.** The decoder and the builder against literal packet bytes, including the macOS
4-byte AF prefix and the Linux flags+EtherType prefix. The filter grammar, term by term. The
escalation bound, measured as *LLM call counts* against a recording mock while 40 packets are
injected. That a failure writes nothing.

**Not proven — do not claim otherwise.** Interface creation and the real transport have
**never been executed**. No test in this suite has root, so `tun::create()` has never run, no
packet has ever crossed a real `utun` or `/dev/net/tun`, and the blocking read/write threads
have never been scheduled. Treat all of `TunTapServer::spawn_with_llm_actions` as unexercised
code.

**Path to Beta.** `sudo` on this machine, a real `utun`, and `ping 10.7.0.2` plus `nc` as the
peer: confirm the host accepts the packets NetGet builds (checksums included — the host's stack
is the only honest checker) and that the read loop and stop signal behave. That has not been
done. Per the root `CLAUDE.md`'s `wireguard` lesson, a maturity rating that rests on a test
which never ran the thing it claims is exactly the bug to avoid — so this stays `Experimental`
until a real peer completes a real exchange.
