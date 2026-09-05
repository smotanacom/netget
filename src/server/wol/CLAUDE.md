# Wake-on-LAN Protocol Implementation

A Wake-on-LAN listener. Magic packets arrive on UDP; NetGet decodes them and the model decides
what, if anything, they mean. **Nothing is ever sent back**, and that single fact is what
shapes the whole implementation.

**State**: `Experimental` — see "Why Experimental" below; the reason is specific and fixable.
**Privilege**: declares `PrivilegedPort(9)`. **Connectionless**: yes.
**Stack**: `ETH>IP>UDP>WOL`. **Dependencies**: none — the decoder is ~40 lines of this file.

## The wire

The magic packet (AMD's Magic Packet Technology) is:

```
FF FF FF FF FF FF        6 bytes of 0xFF - the "sync stream"
<MAC> x 16               the 6-byte target MAC, repeated exactly 16 times
                         = 102 bytes
[password]               optional 4- or 6-byte SecureON password (a vendor extension)
```

It is normally broadcast to UDP port 9 (discard) or 7 (echo), and can also be sent as a raw
Ethernet frame with EtherType `0x0842`.

### The one decoding subtlety: scan, do not assume offset 0

The payload can sit **anywhere inside the datagram** — senders wrap it, pad it, or forward a
whole captured Ethernet frame. `decode_magic_packet` therefore walks every offset looking for
six `0xFF` bytes and validates the sixteen repetitions from there; a candidate that fails does
not end the scan, because a datagram can contain a false sync stream before the real packet.
A decoder that checks only offset 0 silently drops real wake requests, and one that checks only
"does it start with 0xFF x6" accepts near-misses.

What is rejected, and tested against literal bytes in `tests/server/wol/decode_test.rs`:

* **15 repetitions** — the classic off-by-one. A NIC's pattern matcher requires all sixteen, so
  96 bytes of a plausible-looking pattern is not a magic packet. Rejected even when padded out
  to the full 102 bytes, so only the repetition count is wrong.
* **a sync stream that is not exactly six `0xFF`** — `FE FE FE FE FE FE`,
  `FF FF FF FF FF FE`, and so on.
* **one wrong byte in any repetition.**

`FF:FF:FF:FF:FF:FF` (broadcast) is the degenerate case where the sync stream and the MAC are
indistinguishable. It decodes correctly and is tested.

### SecureON

Recognised only when the bytes after the 102 run to the end of the datagram and number
**exactly 4 or 6** — the two lengths the extension defines. Any other trailer is reported as
`password_length: 0`: with the payload allowed at any offset there is no way to tell a password
from padding, and claiming one we are not sure about is worse than reporting none.

**The password bytes are deliberately never exposed** — not to the model, not in the event. A
4-byte password is conventionally an IPv4 address and a 6-byte one a MAC, so a formatted
rendering would have been possible, but a credential someone tried against this listener is not
something the model needs in order to decide anything, and the event carries `has_password` and
`password_length`, which is what a decision actually turns on.

### `transport`, and what NetGet does *not* do

`transport` is `"ethernet"` when the **UDP payload was itself a complete Ethernet frame** with
EtherType `0x0842` (a full 14-byte header, and `08 42` in the two bytes immediately before the
sync stream — a structural check, not a two-byte guess). That is what a relay or a
capture-replay tool produces.

**NetGet never receives a real `0x0842` frame off the wire.** That needs a raw socket, and the
`wol` feature deliberately carries no packet-capture dependency — it has no dependencies at all.
If the link-layer form is ever wanted, it belongs with `arp`/`datalink`/`isis` and their `pnet`
dependency, and `privilege_requirement` would have to become `RawSockets`.

## The honest design problem: there is no response

Wake-on-LAN is one-way. A target NIC that recognises its own MAC pulls the machine's power up
and **says nothing** — no acknowledgement, no status, no transaction id. There is nothing for
the model to author.

So this protocol offers the model **three** actions, and the shortness of that list is the
point rather than an omission:

| Action | What it decides | Wire effect |
|---|---|---|
| `record_wake_request` | this packet is for a host I recognise; keep it, with a label | none |
| `ignore_magic_packet` | it is not; drop it, and say why | none |
| `announce_host_awake` | **not part of Wake-on-LAN** — see below | one UDP datagram, if enabled |

`get_async_actions()` returns **nothing**, and that is deliberate: an async action is a
user-triggered verb needing no network context, and a listener that never transmits has none.
The one action that does transmit needs both the per-server gate and the magic packet's source
address, neither of which exists outside the receive loop.

The root `CLAUDE.md` warns that offering the model *no* vocabulary is the worst case, not the
exempt one (`usb-fido2` shipped three events the model could not answer). That is why all three
actions are attached to the event with `.with_actions(...)` and all three are executable:
the model is never asked a question it has no words to answer, even when the honest answer is
"record it this way" or "stay silent".

### `announce_host_awake` — named plainly, off by default

Wake-on-LAN has no reply, so this action is a NetGet extension for lab and honeypot use: it
sends a plain UDP text datagram claiming the host is now awake, either to an explicit
`announce_to` or — with no `announce_to` — back to whoever sent the magic packet.

* It is **off unless** the server was started with `allow_non_standard_ack: true`.
* When off, the action is **refused and logged**; nothing is transmitted.
* Its own description, its log template and this file all say it is not part of the protocol.

The send happens in `WolServer::process_announcements`, not in `execute_action`, for two
reasons: the gate is a per-server startup parameter the stateless protocol struct cannot see,
and the default destination is the packet's source address, which only the receive loop knows.
`execute_action` therefore validates the action and returns `NoAction` — so the datagram goes
out exactly once, from the one place that can decide whether it may go out at all. Its log
template says "requested", not "announced", because it fires before the gate is consulted.

## Startup parameters

| Parameter | Type | Default | Read at |
|---|---|---|---|
| `allow_non_standard_ack` | boolean | `false` | `WolServer::spawn_with_llm_actions` |

That is the whole list. A declared parameter nothing reads is an advertised knob that does
nothing when turned (`startup_param_drift_test` fails the build on one), so there is no second
parameter waiting to be wired up.

## Events

One event, `wol_magic_packet_received`, raised **only** for a datagram that decodes as a magic
packet. Fields: `target_mac` (formatted `00:11:22:33:44:55`, never bytes), `source_address`,
`has_password`, `password_length`, `transport`, `sync_offset`.

A datagram that is *not* a magic packet raises nothing and is logged at DEBUG. Port 9 is the
discard port and attracts scanners; there is no decision for the model to make about a stray
datagram, so paying for an LLM call on one would be pure cost.

## Failure behaviour: the log is the only place the silences differ

Because there is no reply, **four different outcomes are byte-identical on the wire — all four
are silence**:

1. the model recognised the host,
2. the model deliberately dropped the packet,
3. the model answered with nothing that decides the packet,
4. the LLM call failed.

The first two are decisions. The last two are NetGet failing to make one. An LLM failure being
silent here is *protocol-correct* — there is no error frame to send, and no `WireFailure` reply
belongs on this protocol — but silence that is correct and silence that is an outage must be
distinguishable after the fact. Following `src/server/radius/`, every magic packet produces
exactly one greppable line:

```
decision=model_accept            record_wake_request or announce_host_awake
decision=model_reject            ignore_magic_packet
decision=model_silent            the call succeeded and decided nothing
decision=fail_closed_llm_error   the call failed
    category=overloaded|unavailable   from crate::utils::WireFailure::classify
```

`category=` is split out so a transient overload is not recorded as a permanent fault.
The error text goes to the log and the operator status stream **only** — there is no wire to
leak it onto, which is the one advantage of a protocol with no reply.

## Not implemented

Raw `0x0842` frame capture (see above); actually waking anything (NetGet is not a NIC and has
no host to power on); SecureON password verification (there is nothing to verify against, and
verifying would be storage); WOL over IPv6 multicast; port 7 is not special-cased — it is just
another port to bind.

**No storage of any kind.** `record_wake_request` writes nothing to disk or to a database: the
durable record is the server's access log entry, which already contains the action verbatim and
is readable with `list_access_logs` / `get_access_log`.

## Why `Experimental`

The magic-packet format is small enough to validate exhaustively against literal bytes, and
that is done. What is missing is the only evidence that would justify `Beta`: **a packet
produced by a third-party sender.** No `wakeonlan`, `etherwake`, `ether-wake` or `wol` binary is
installed on this machine, the Python `wakeonlan` module is absent, and adding a Rust WoL crate
would mean editing `Cargo.toml`. So every packet in the test suite is *this repository reading
the specification for itself* — an independent reading, not an independent implementation. That
is exactly the `dhcp` situation the root `CLAUDE.md` describes, and it is `Experimental`, not
`Beta`.

Promoting it is cheap and the path is specific: install `wakeonlan` (or `etherwake`), point it
at `127.0.0.1` on a high port, and assert the decode — in a test that **hard-fails when the
binary is missing**, the way `npm`'s does. A `SKIP: … is not installed` that returns `Ok(())`
would leave the rating resting on nothing, which the root `CLAUDE.md` lists as its own
near-miss category.

Also unproven, and worth a human's eye before this goes past `Beta`: the `transport: "ethernet"`
path has never seen a datagram produced by a real relay, only ones this repository assembled;
and the protocol has never run on port 9 with privileges, so the `PrivilegedPort(9)` preflight
has been exercised only by not firing.

## Example prompts

```json
{"type": "open_server", "port": 9, "base_stack": "wol",
 "event_handlers": [{"event_pattern": "wol_magic_packet_received", "handler": {"type": "script",
   "language": "python",
   "code": "import json,sys\nd=json.load(sys.stdin)\ne=d['event']\nknown={'00:11:22:33:44:55':'lab-nas'}\nmac=str(e.get('target_mac','')).upper()\nif mac in known:\n    a=[{'type':'record_wake_request','target_mac':mac,'host':known[mac]}]\nelse:\n    a=[{'type':'ignore_magic_packet','reason':'unknown target MAC'}]\nprint(json.dumps({'actions':a}))"}}]}
```

```
Wake-on-LAN on port 9. Record magic packets for 00:11:22:33:44:55 as 'lab-nas' and
ignore every other MAC.
```

```
listen via wol on port 9040. Treat any packet carrying a SecureON password as suspicious
and note who sent it; ignore the rest.
```

## Verified

With a script handler (zero LLM calls) the five decoding shapes all report correctly:

```
Wake-on-LAN magic packet for 00:11:22:33:44:55 from 127.0.0.1:… (offset 0,  udp,      password_length=0)
Wake-on-LAN magic packet for 00:11:22:33:44:55 from 127.0.0.1:… (offset 20, udp,      password_length=0)
Wake-on-LAN magic packet for 00:11:22:33:44:55 from 127.0.0.1:… (offset 0,  udp,      password_length=4)
Wake-on-LAN magic packet for 00:11:22:33:44:55 from 127.0.0.1:… (offset 0,  udp,      password_length=6)
Wake-on-LAN magic packet for 00:11:22:33:44:55 from 127.0.0.1:… (offset 14, ethernet, password_length=0)
```

See `tests/server/wol/CLAUDE.md` for what the e2e suite asserts and what it costs.
