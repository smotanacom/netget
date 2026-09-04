# CAN bus (SocketCAN)

An LLM-driven ECU simulator: the model decides how a simulated engine controller, gateway or
body-control module answers what it hears on a vehicle bus.

**State**: `Experimental`, and the reason is specific — see
[Maturity](#maturity-and-what-would-earn-beta). **The SocketCAN transport has never been
compiled or run.**
**Privilege**: `PrivilegeRequirement::None`. **Connectionless**: declared.
**Stack**: `CAN`. **Spec**: ISO 11898-1:2015 (classic CAN and CAN FD),
`Documentation/networking/can.rst` in the Linux kernel tree for SocketCAN.

## THE PLATFORM RULE

`AF_CAN` is an address family implemented **only in the Linux kernel**. It is not a library that
could be ported: it comes with a queueing discipline, a per-interface error-confinement state
machine, and the `can_frame`/`canfd_frame` structs the network stack passes around. macOS has no
equivalent — no `AF_CAN`, no `can0`, no `cansend`. Neither does Windows. A CAN adapter on those
platforms is reached through a vendor's userspace USB driver speaking a proprietary protocol,
which shares nothing with this code.

| Platform | `AF_CAN`? | Behaviour |
|---|---|---|
| Linux | Yes | Opens a real CAN socket |
| macOS | **No** | `spawn()` returns `Err` naming the reason |
| Windows | **No** | `spawn()` returns `Err` naming the reason |

**The protocol still compiles everywhere, still registers, and is still visible to the model.**
`socketcan` is declared inside the existing `[target.'cfg(target_os = "linux")'.dependencies]`
block, exactly like `bluer`, so the `can` feature resolves on every platform and the crate is
simply absent off Linux. Every use of it is behind `#[cfg(target_os = "linux")]`.

Refusing is not the same as hiding, and the difference is the whole design:

> *"Hiding a protocol is not the same as refusing to start it. Hidden, the model never learns
> why; refused, the user gets `ServerStatus::Error` with the reason."*

`transport::UNSUPPORTED_PLATFORM_MESSAGE` is the single source of that text — the runtime error,
the protocol documentation and the test all quote the same `const`, so they cannot drift.
`tests/server/can/e2e_test.rs::the_socketcan_transport_refuses_to_start_off_linux` asserts the
error is byte-identical to it.

## Files, and why they are split this way

| File | What it is |
|---|---|
| `frame.rs` | The **pure** codec: representation, validation, DLC tables, the SocketCAN struct layout, error-class and bus-state decoding, action parsing. No socket, no `cfg`, no state. |
| `transport.rs` | The thin OS layer: `FrameSink`, the platform refusal, and the `#[cfg(target_os = "linux")] mod linux` that is never compiled here. |
| `actions.rs` | `Protocol` + `Server`: two actions, three events, metadata, startup parameters. |
| `mod.rs` | Both transports' loops, event dispatch, and the `decision=` logging. |

**The split is the point.** The transport cannot be executed on the machine that wrote it, so it
is kept as thin as it can be — `transport.rs`'s Linux module is roughly 120 lines and contains no
decisions, only conversions. Everything that decides *what the bytes are* lives in `frame.rs`,
which is pure and is asserted against literal values in `tests/server/can/frame_test.rs`. This is
the `bluetooth_ble_beacon` precedent the root `CLAUDE.md` describes: *"payload construction is
pure and exhaustively unit-tested against literal spec bytes; its BlueZ transport has never been
compiled or run"*. `metadata().notes` says which half is which, which is what `Experimental` is
for.

Keep `frame.rs` pure. The moment it reads a socket or a config value, the only provable part of
this protocol stops being provable.

## Transports

### `transport: "socketcan"` (default)

A real `AF_CAN` socket on a Linux CAN interface, via the `socketcan` crate. **Never compiled,
never executed.** Written against the `socketcan` 3.6 sources (`src/socket.rs`, `src/frame.rs`)
rather than from memory, and it is written to the same shape as `lldp`'s pcap path, including the
parts that path learned the hard way:

- **Startup reports failure.** The socket is opened inside `spawn_blocking`, but the outcome comes
  back over a `oneshot` and `spawn()` returns `Ok` only once it is genuinely open. ARP, DataLink
  and ICMP each shipped the fire-and-forget version and were each fixed separately.
- **Two sockets, not one shared.** A transmit blocked while the controller arbitrates for the bus
  must not stall the receive loop. `lldp` opens two pcap handles for the same reason.
- **Stopping is cooperative.** `JoinHandle::abort()` cannot interrupt a thread parked in a
  blocking read, so the loop polls a `StopSignal` and `read_frame` has a 500 ms timeout that
  bounds how long a stop takes to notice on a silent bus.
- **`set_error_filter_accept_all()` is required and is not the default.** A raw CAN socket's error
  mask starts at zero, so without it the kernel delivers no error frames and `can_error_frame` and
  `can_bus_state_changed` could never fire — an event declared and never emitted, which this
  repository has shipped in bulk before.
- **`recv_own_msgs` stays off** (the default). With it on, every frame this server transmits comes
  back as a received frame, raising an event, producing another frame, forever. `lldp` guards the
  same loop by filtering on its own source address; here the kernel does it.

**The blocking API is used deliberately.** `socketcan`'s `tokio` feature is *not* enabled in this
tree (`Cargo.toml` declares `socketcan = { version = "3.5", optional = true }` with default
features, and the lockfile confirms no `tokio`/`mio`/`futures`), so `socketcan::tokio` does not
exist here. If someone later wants the async API, the line to change is
`socketcan = { version = "3.5", optional = true, features = ["tokio"] }` — but `spawn_blocking`
plus a read timeout is the same arrangement `lldp` and `arp` use for pcap and needs no dependency
change.

### `transport: "udp"` (testing)

Carries the exact octets an `AF_CAN` socket would — a 16-octet `struct can_frame` or a 72-octet
`struct canfd_frame` — as a UDP datagram payload. No real CAN node speaks it, and **no assertion
made over it says anything about SocketCAN.**

What it does buy is everything else: the codec, the events, handler and script dispatch, the model
call, the action executor and the frame builder all run, unprivileged, on a machine with no CAN
stack. It is a **declared startup parameter** rather than a test-file convention — the same
accommodation `lldp` makes for its raw-Ethernet transport, made visible in
`get_startup_parameters()` where an operator can see what it is and the description says twice
that nothing real speaks it.

## Startup parameters

Three declared, three read, all in `CanConfig::from_params`.

| Parameter | Default | What reads it |
|---|---|---|
| `interface` | `can0` (or `SpawnContext::interface`) | The CAN interface to bind; also the event's `interface` field |
| `transport` | `"socketcan"` | Selects `spawn_socketcan` or `spawn_udp` |
| `udp_peer` | last peer heard from | Destination on the UDP transport before anything has been received. **Rejected with `transport: "socketcan"`**, where it would do nothing |

Everything else is refused by name at startup, including `bitrate` — which is set with
`ip link set can0 type can bitrate 500000` before NetGet runs, not by NetGet.

Errors propagate with `?`, never `unwrap()`, and parameters are parsed **before** anything is
opened or bound.

## Privilege: `None`, deliberately

Binding an `AF_CAN` socket needs no privilege. What needs privilege is bringing the *interface*
up and creating a `vcan` device, and both happen before NetGet runs — so `spawn()` fails with the
`ip link` command to run rather than the protocol refusing to start for everyone.

Declaring `Root` here would be the mistake `ospf` made in the other direction: it would refuse to
start for every user who can in fact open the socket. There is no `PrivilegeRequirement` variant
for "a device must exist", and inventing one is out of scope (root `CLAUDE.md`, IMPROVEMENTS
item 60).

## What the model sees and controls

### Events

| Event | When | Emit site |
|---|---|---|
| `can_frame_received` | A data or remote frame arrived | `handle_frame` |
| `can_error_frame` | The controller reported an error condition | `handle_frame`, when `CAN_ERR_FLAG` is set |
| `can_bus_state_changed` | The controller crossed a confinement boundary | `handle_frame`, on a **transition** only |

All three carry the full action set via `.with_actions(...)`, and all three have real emit sites
(`tests/event_emit_sites_test.rs` guards that).

`can_bus_state_changed` fires only on a genuine transition — the state is tracked in
`CanContext::bus_state` and compared before the event is built. A degrading bus emits the same
error frame repeatedly, and raising the event each time would flood the model with reports that
say nothing new. An error frame that reports a state change raises **both** events, because they
answer different questions and an operator's handler may reasonably key on either.

### Actions

| Action | Effect |
|---|---|
| `send_can_frame` | Builds and transmits one frame from structured fields |
| `no_response` | Say nothing, deliberately. Logged `decision=model_reject` |

`send_can_frame` is validated **at the action** by running the encoder, so a 12-bit identifier on
a standard frame, an `rtr` + `fd` combination, a `brs` without `fd` or a 9-byte CAN FD payload is
refused with the model watching, instead of producing something the bus silently drops.

### `data` is hex by default, and that is unusual here on purpose

The project rule is *never put raw bytes or base64 in action parameters* — use structured fields.
**CAN is the legitimate exception, and the reason is that the structure genuinely is not in the
protocol.** Eight octets mean whatever a DBC file says they mean: engine speed as a 16-bit
big-endian value scaled by 0.25 starting at bit 24, say. NetGet does not have the DBC file, and
could not, because it is a database owned by whoever built the vehicle. There is nothing to expose
as fields and no text interpretation to fall back to, so `"hex"` is the **default** encoding —
which no other protocol in this tree does.

`"text"` is still offered, because ASCII payloads do occur (a VIN in a UDS response, some
telematics gateways) and writing `"484921"` for `"HI!"` helps nobody. A JSON array of byte values
is also accepted, because a script handler naturally produces one.

**The executor really decodes what `encoding` says, and never sniffs.** `"48656c6c6f"` is
simultaneously valid text and valid hex and only the sender knows which it meant. The root
`CLAUDE.md` records `send_tcp_data` documenting hex in three places while its executor called
`as_bytes()`, so a model following the documentation put literal ASCII on the wire; that is
exactly the shape this avoids, and
`frame_test.rs::text_and_hex_are_different_encodings_of_the_same_string` pins the difference.

The identifier is also written in hex (`"0x7DF"`, `"7DF"`, or a decimal number), which is not a
violation for the same reason a UUID is written in hex: it is a fixed-width numeric identifier in
its published notation, and `parse_id` really parses it. A **bare** string is read as hex, because
that is how every CAN document, DBC file and `candump` line writes an identifier — reading
`"0700"` as seven hundred decimal would address a different ECU.

`extended` is a field, never inferred from the identifier's magnitude. `0x123` is legal in both
formats and they are **different frames** that different nodes answer.

### The CAN FD DLC is a length code, not a byte count

This is the classic implementation error in every CAN stack, so `frame.rs` writes the table out
(`FD_DLC_TO_LEN`) and `dlc_for_len` / `len_for_dlc` are separate named functions:

| DLC | 0–8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 |
|---|---|---|---|---|---|---|---|---|
| CAN FD bytes | 0–8 | 12 | 16 | 20 | 24 | 32 | 48 | 64 |
| Classic bytes | 0–8 | 8 | 8 | 8 | 8 | 8 | 8 | 8 |

A CAN FD frame has **no encoding for 9, 10, 11, 13… bytes**. Those are refused rather than
rounded: padding up appends bytes the model did not write, and truncating down drops bytes it did.
All sixteen codes are pinned against literals in both directions.

## LLM failure → silence. There is no alternative here.

CAN is in the **deliberately-silent** class the root `CLAUDE.md` catalogues, and it is the
clearest case in the tree because the alternative is actively destructive.

**CAN has no error reply.** The thing called an error frame is not a message: it is six dominant
bits transmitted *on top of* a frame in flight, which destroys that frame for every node on the
bus. The transmitting controller then increments its error counter; enough of them make it
error-passive, and past 255 it goes bus-off and disconnects itself entirely. Emitting one to
signal "NetGet's backend is down" would corrupt somebody else's traffic and could take a node off
the bus.

So:

- **Nothing in the action vocabulary can emit an error frame.** There is no `error` parameter, and
  `transport::linux::to_socketcan` cannot construct a `CanAnyFrame::Error`.
  `e2e_test.rs::no_action_can_emit_an_error_frame` asserts both.
- An LLM failure transmits **nothing**, and no `WireFailure` text ever reaches the bus.

Silence is also completely ordinary on CAN — almost every node ignores almost every frame — so
all outcomes are indistinguishable on the wire and the difference survives only in the log, tagged
the way `src/server/radius/` tags its own:

| Tag | Meaning |
|---|---|
| `decision=no_policy` | No instruction and no handler — nothing transmitted, and **no LLM call at all** |
| `decision=model_reject` | The model answered `no_response` — a real decision |
| `decision=model_silent` | The model returned nothing usable |
| `decision=fail_closed_overloaded` | The call failed and `WireFailure::classify` says the backend is saturated (retryable) |
| `decision=fail_closed_llm_error` | The call failed otherwise |

### Default behaviour: listen, no LLM

With no operator policy (no server instruction, no event handler) the server observes and says
nothing, **without** a model round-trip per frame — `operator_wants_dynamic` in `mod.rs`, the same
gate `lldp`, `arp` and `ospf` use. This matters more here than anywhere else in the tree: a real
CAN bus carries thousands of frames a second, and a model call per frame would be ruinous as well
as pointless.

## Security: no addressing, no authentication

**Every frame reaches every node.** There is no source address, no destination address, no
sequence number, no checksum over anything an attacker cannot recompute, and no authentication of
any kind. Arbitration is by identifier — the numerically lowest wins the bus — which is a priority
scheme, not a security one. A node decides entirely for itself whether an identifier is one it
answers.

So a NetGet instance on a real vehicle bus **can impersonate any ECU**, and nothing on the bus can
tell. It can answer a diagnostic request as the engine controller, or transmit the frame the
instrument cluster reads its speed from.

That is the point and it is the hazard. It is what makes an ECU simulator possible at all — a
genuinely underserved niche, since the alternative is a hardware bench with a real controller on
it — and it is why this must only ever be pointed at a bench, a `vcan` interface, or a vehicle you
own. Frames NetGet transmits are indistinguishable from the real ECU's.

## Maturity, and what would earn Beta

`Experimental`, precisely:

- **Proven**: the codec, against literal values — both DLC tables in both directions, all sixteen
  CAN FD length codes, standard vs extended identifiers, RTR, BRS/ESI, error-class decoding, the
  confinement ladder, both struct layouts octet by octet, and an over-length or unencodable
  payload being refused rather than truncated. Plus the whole event → handler/LLM → action → frame
  path over the UDP transport, in-process, including that an LLM failure transmits nothing.
- **Not proven**: the `AF_CAN` transport. It has never been compiled — the machine that wrote it
  is macOS and has no Linux target installed — let alone run. No frame this code produced has
  reached a real CAN bus or a real CAN peer.

The bar for Beta, per the root `CLAUDE.md`, is *a real independent peer completing a real
exchange*. **Here the experiment is genuinely cheap and needs no hardware**, which is why this is
worth writing down rather than deferring:

```bash
# A fully functional virtual CAN bus, no hardware:
sudo modprobe vcan
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0

sudo apt install can-utils          # cansend, candump — real third-party peers

netget --server can --startup-params '{"interface":"vcan0"}' \
       --instruction 'You are an engine ECU: answer OBD-II mode 01 requests on 0x7DF from 0x7E8'

candump vcan0 &                     # watch the bus
cansend vcan0 7DF#0201050000000000  # a real tester's OBD-II request
```

Two directions have to hold before the rating moves: `candump` shows the frame the model authored,
with the identifier, DLC and payload exactly as written; and a `cansend` from `can-utils` arrives
as a `can_frame_received` event with matching fields. `can-utils` is a genuinely independent
implementation — it is the reference userspace for SocketCAN and shares no code with this crate —
so it satisfies the rule that `websocket`/`webrtc_signaling` fail by being driven with the same
library the server frames with.

For CAN FD, add `sudo ip link set vcan0 mtu 72` and use `cansend vcan0 123##1<data>`.

**None of this has been run.** It needs Linux, which no agent here has. Do not promote on the
codec tests alone — that is the mistake `wireguard` made, and the correction is recorded in the
root `CLAUDE.md`.

## Not implemented

- **No higher layer.** ISO-TP (ISO 15765-2) segmentation, UDS (ISO 14229), OBD-II PID semantics
  and J1939 are all *not* implemented. The model sees single frames and answers with single
  frames; a multi-frame UDS response would have to be composed as several `send_can_frame` actions
  with the flow control done by the model. `CAN_ISOTP` and `CAN_J1939` are separate SocketCAN
  protocol families and would be separate NetGet protocols.
- **No DBC.** Payloads are opaque bytes, by design — see above.
- **No CAN filters.** Every frame on the bus reaches the event dispatcher. On a busy bus that is a
  lot; `set_filters` on the socket is the obvious next feature and would cut the volume before it
  reaches NetGet at all.
- **No transmit-side error handling beyond logging.** A `write_frame` failure is logged; nothing
  retries or backs off.
- **`len8_dlc`** (a raw DLC above 8 on a classic frame, which some controllers allow) is decoded as
  8 bytes and never produced.
- **No storage of any kind.** The model invents every response.

## Example prompts

```
Act as an engine ECU on vcan0: answer OBD-II mode 01 requests on 0x7DF or 0x7E0 from 0x7E8.
Report 2400 rpm for PID 0C and 88 degrees coolant for PID 05. Ignore every other identifier.
```

```json
{"type": "open_server", "base_stack": "can",
 "startup_params": {"interface": "vcan0"},
 "event_handlers": [{"event_pattern": "can_frame_received", "handler": {"type": "static",
   "actions": [{"type": "send_can_frame", "id": "0x7E8", "data": "0341051e",
                "encoding": "hex"}]}}]}
```

```
CAN honeypot on vcan0: log every frame you see and transmit nothing (no_response for
everything).
```
