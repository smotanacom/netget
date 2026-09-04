# M3UA / SIGTRAN Server Implementation

MTP3 User Adaptation Layer, [RFC 4666](https://datatracker.ietf.org/doc/html/rfc4666). NetGet
plays the **SGP** (signalling gateway process); the peer is an **ASP** (application server
process). Rust owns the association and the state machine's mechanics; the model owns the two
admission decisions and the SS7 traffic.

**Status**: `Experimental`, **and it cannot be otherwise from macOS** — see
[The transport problem](#the-transport-problem-read-this-first). Nothing here has ever run over
the transport the protocol actually uses.

**Port**: SCTP 2905 (IANA). Above 1023, so `PrivilegeRequirement::None` — SCTP is an ordinary
kernel transport like TCP, not a raw socket.

## Layout

| File | Role |
|---|---|
| `codec.rs` | Common header, TLV parameters, Protocol Data, message builders. Pure functions, `pub`, asserted against literal RFC octets. |
| `mod.rs` | Transport selection, listener, association loop, ASP state machine, LLM dispatch. |
| `actions.rs` | Action definitions and validation, event types, metadata, startup parameters. |

## The transport problem (read this first)

**M3UA runs over SCTP.** RFC 4666 section 1.4.1 requires it and IANA assigns SCTP port 2905.
**macOS has no SCTP stack at all** — no kernel support, no headers — and macOS is the machine
this was written and tested on. `PROTOCOL_ROADMAP.md` records three ways out; two are
implemented and the third is recorded rather than pretended.

### 1. SCTP is the default, and a host without it is refused by name

`bind_listener` asks the kernel for a `SOCK_STREAM` / `IPPROTO_SCTP` (132) socket through
`socket2`. Where that fails, `spawn()` returns an `Err` and `server_startup` puts the server in
`ServerStatus::Error`. The message names SCTP, names RFC 4666, and names the escape hatch,
because an errno alone (`Protocol not supported`) tells an operator nothing about what to do
next.

This is the `bluetooth_ble_beacon` precedent from the root `CLAUDE.md`: **hiding a protocol is
not the same as refusing to start it.** Hidden, nobody learns why; refused, the reason is on
the screen.

It is a **probe, not a `cfg`**. The socket is genuinely requested, so a Linux kernel with the
`sctp` module unloaded is diagnosed exactly like macOS, and a Linux kernel that has it gets a
real SCTP listener with no code change. That choice follows the privilege model's own lesson:
`SystemCapabilities` used to *infer* raw-socket access and the check therefore never fired.

Compiling the SCTP path unconditionally (rather than behind `#[cfg(target_os = "linux")]`) is
also deliberate. Code that only compiles on a platform nobody builds for is code nobody
compiles; this way the macOS build type-checks every line of it, and the only thing that
differs at runtime is whether `socket(2)` succeeds.

### 2. `transport: "tcp"` is a lab affordance, labelled as one everywhere

The startup parameter accepts `"tcp"`. **No real SIGTRAN peer speaks M3UA over TCP.** It exists
so the layers above the transport can be exercised on this machine at all.

Because a label like that is only useful if it travels, `M3uaTransport::label()` returns
`"tcp (NON-STANDARD lab transport, not SIGTRAN)"` and that string appears in:

- the startup parameter's description, which is what the model reads;
- `metadata().notes`;
- the startup log line, plus a separate WARN saying it in full;
- the connection's `protocol_info`, so the dashboard rail shows it on the row;
- **every event's `transport` field**, so a handler and the model see it too.

`transport_test.rs` asserts each of those, including that the declared example is `"sctp"` — if
the default ever flipped, an operator on a host *with* SCTP would silently get the lab framing.

Everything above the socket is transport-agnostic: M3UA is length-delimited, so the framing does
not change. That is what makes the lab transport useful, and exactly why it must be labelled —
the code being exercised is real and the transport under it is not.

### 3. Userspace SCTP — the real fix, not done here

`webrtc-rs` is already in the tree and carries an SCTP implementation, so a userspace
association is not far-fetched. It is a project of its own: association setup, path management,
multi-streaming and multi-homing, none of which falls out of the M3UA work. Recorded so it is
not re-litigated from scratch.

**The route to real validation is `osmo-stp` / `libosmo-sigtran` on Linux**, where the SCTP path
above can actually execute. Until somebody does that, no claim of interoperability is available
and the rating cannot move past `Experimental`.

## Codec: the two rules that are usually got wrong

**Parameter padding is excluded from the Parameter Length** (RFC 4666 section 3.2). A parameter
whose value is the two octets `"ok"` declares length **6** and occupies **8** octets. This is the
single most common M3UA implementation error, and it is invisible to a round-trip test: an
implementation that writes 8 into the length field decodes its own messages perfectly and
corrupts every real peer's. `tests/server/m3ua/codec_test.rs` therefore asserts **literal octets
written by hand from the RFC**, never `encode(decode(x)) == x`.

The consequential case is Protocol Data. A 15-octet value (12 of routing label plus 3 of user
part) declares 19 and occupies 20. Get it wrong by one and the far end reads a padding octet as
the last byte of an ISUP message.

**Message Length covers the whole message including the common header.** NetGet *includes* the
parameters' padding in it, which is what the reference stacks do. A peer that excludes the final
parameter's padding produces a length that is not a multiple of four while still writing the
padding octets, so `read_message` drains `codec::alignment_slack(length)` octets afterwards —
otherwise the next header starts mid-word. NetGet's own messages always leave zero slack.

## Session

```text
accept                       -> ASP-DOWN
ASPUP  -> m3ua_asp_up_received     -> ASPUP ACK  -> ASP-INACTIVE  (or ERR, stay DOWN)
ASPAC  -> m3ua_asp_active_received -> ASPAC ACK  -> ASP-ACTIVE    (or ERR, stay INACTIVE)
DATA   -> m3ua_data_received       -> optional DATA back
BEAT   -> BEAT ACK                                (Rust, no LLM call)
ASPIA  -> ASPIA ACK                -> ASP-INACTIVE (Rust, no LLM call)
ASPDN  -> ASPDN ACK, then m3ua_asp_down_received  -> ASP-DOWN
ERR    -> m3ua_error_received                     (observational)
```

### What is answered in Rust, and why

- **BEAT → BEAT ACK.** A keepalive is not a decision. Routing it through the model would spend a
  request per heartbeat per association to decide nothing; routing it through a *parked manual
  handler* would drop the association while a human read the question. The Heartbeat Data is
  echoed verbatim — that opaque token is the entire point of the parameter.
- **ASPDN → ASPDN ACK** and **ASPIA → ASPIA ACK.** Taking a peer *out* of service is the safe
  direction and needs no permission. An ASP that cannot leave cleanly is a worse failure than one
  that cannot join.
- **Every protocol-validity refusal**: a bad version, a length below the header, a malformed TLV,
  ASPAC while ASP-DOWN, DATA while not ASP-ACTIVE, a routing context or network appearance that
  does not match the configured one. These follow from the protocol, not from policy, so the
  model is never consulted about them.
- **A duplicate ASPUP or ASPAC** for a peer already in that state is re-acknowledged without
  asking again (RFC 4666 section 4.3.4.3). It is not a new admission — this ASP was admitted
  already.

### Concurrency

The stream is split with `tokio::io::split`; the write half lives behind an `Arc<Mutex<..>>`
shared with the dashboard's peer command task. The mutex is held only for the duration of a
socket write and **never across an LLM call** — the model round-trip happens with nothing
locked, and the resulting bytes are written afterwards. There is no keepalive ticker, so no
writer task is needed (BGP needs one because its timers must keep sending while the read loop is
inside an LLM call).

## Fail closed: the whole point

An ASPUP is a request to join a **signalling network**. There is no safe default answer, so
every path that is not an explicit acknowledgement ends in `ERR` and leaves the ASP where it
was. The four non-answers are kept apart in the log because collapsing any two of them is a bug:

| Situation | Wire | Log tag |
|---|---|---|
| `send_m3ua_asp_up_ack` / `send_m3ua_asp_active_ack` | that ACK, ASP advances | — |
| Policy refused with `send_m3ua_error` | that ERR, ASP unchanged | `decision=model_reject` |
| Policy ran and produced no ACK (`wait_for_more`, empty handler, a NTFY and nothing else) | ERR `0x0d` | `decision=model_silent` |
| Policy could not be run (backend error, timeout, saturation) | ERR `0x0d` | `decision=fail_closed_llm_error` |
| No policy exists at all | ERR `0x0d` | `decision=no_policy_configured` |

**This is where M3UA deliberately parts company with BGP.** BGP treats "no instruction and no
handler" as a static default and completes the handshake, reasoning that the operator opened the
port with that ASN. M3UA does not: `operator_wants_dynamic() == false` is a refusal, because
"who may join this signalling gateway" has no configuration-free answer that is not a guess. The
log line names what is missing rather than leaving an operator to wonder why nothing works.

In practice every instance created through the dashboard has a policy — `ServerForm::create`
substitutes an instruction and adds a `*` → manual rule — so the refusal is reached only by a
programmatic `open_server` that supplied neither.

**On `m3ua_data_received` the mapping is different**, and the difference is the point: a policy
that ran and chose to say nothing sends nothing (`decision=model_silent`), because M3UA needs no
reply to a DATA. But a policy that could not be *run* answers ERR, because staying silent would
tell the ASP its MSU reached the SS7 network. A fabricated delivery confirmation is worse than a
refusal.

**Nothing derived from an error reaches the peer.** ERR carries a numeric Error Code and the
optional Diagnostic Information parameter is left empty — it is the one field in M3UA where an
internal string could leak, and `codec::error` never populates it. The category comes from
`crate::utils::WireFailure`; M3UA's error code registry has no resource-exhaustion code, so both
categories map onto `0x0d Refused - Management Blocking` on the wire and are separated in the
log instead. That is the rule the root `CLAUDE.md` states for exactly this case.

## What is deliberately not implemented

- **Routing key management (RKM).** REG REQ / DEREG REQ are decoded and named, and answered with
  ERR `0x04 Unsupported Message Type` plus a log line saying why. Dynamic registration means
  keeping a routing key table, and a routing key table is **storage**, which the root `CLAUDE.md`
  forbids a protocol from implementing. Accepting and forgetting would be worse than refusing:
  an ASP that believes it registered a key will send traffic for it. Configure `routing_context`
  instead, or use the generic SQLite facility from a handler.
- **SSNM generation.** NetGet never emits DUNA/DAVA/SCON/DUPU — there is no MTP3 network
  underneath to report on. An SSNM message *arriving* is an ASP speaking out of turn and earns
  ERR `0x06`.
- **Multi-homing, multi-streaming, stream identifier selection.** These are SCTP properties; the
  one-to-one socket API exposes none of them, and none can be exercised here anyway.
- **Application Server state, and load-sharing across ASPs.** Each association is independent.
  Traffic Mode Type is echoed as the handler chooses; nothing balances anything.

## Events and actions

| Event | Fires | Useful reply |
|---|---|---|
| `m3ua_asp_up_received` | ASPUP from an ASP-DOWN peer | `send_m3ua_asp_up_ack` to admit, `send_m3ua_error` to refuse |
| `m3ua_asp_active_received` | ASPAC from an admitted peer | `send_m3ua_asp_active_ack` to activate, `send_m3ua_error` to refuse |
| `m3ua_data_received` | an MSU from an active peer | `send_m3ua_data` or `wait_for_more` |
| `m3ua_asp_down_received` | ASPDN, already acknowledged | `send_m3ua_notify` or `wait_for_more` |
| `m3ua_error_received` | the ASP reported an error | observational |

Every event carries the full action list (`call_llm` builds the model's tools from
`EventType::actions`, not from `get_sync_actions()`), and every one has a real emit site.
`get_async_actions` is empty: an async action carries no connection, and every M3UA verb is
addressed to one association. The dashboard's `[ message this peer ]` reaches a live ASP through
the generic peer command channel, which runs exactly the same sync actions.

Actions return `ActionResult::Output` with the fully encoded message, and the session reads the
class and type back off those octets with `codec::peek_class_type` to drive the state machine.
That is deliberate: the state machine follows what is actually on the wire rather than a
parallel bookkeeping that could drift from it. (BGP needs the `Custom` intent indirection because
its encoding depends on the negotiated AS width; nothing in M3UA depends on the session.)

`close_connection` is executable but **not** advertised to the model — refusing an ASP is
`send_m3ua_error`, which leaves the association up so the peer can retry. The dashboard's
`[ disconnect this peer ]` injects it.

### Structured fields, and the one opaque one

The routing label reaches the model as `opc`, `dpc`, `si`, `si_name`, `ni`, `mp`, `sls` — never
as a blob. The SS7 user part is the one legitimately opaque field, and it carries an explicit
`encoding` (`"utf8"` / `"hex"`) in both directions, which `send_m3ua_data`'s executor really
decodes. It is **declared, never sniffed**: `"48656c6c6f"` is simultaneously valid text and
valid hex and only the sender knows which it meant. This is the `send_tcp_data` lesson from the
root `CLAUDE.md` — documenting hex and then calling `as_bytes()` puts literal ASCII on the wire.

Error codes, traffic modes and NTFY status values are accepted by **name**
(`refused_management_blocking`, `loadshare`, `as_active`) as well as by number, and an
undefined numeric value is rejected as a failed action naming the allowed set rather than put on
the wire where no peer can interpret it. Status Information is validated *against its Status
Type*, since type 1 / info 3 is `AS-ACTIVE` and type 2 / info 3 is `ASP Failure`.

## Startup parameters

| Name | Default | Notes |
|---|---|---|
| `transport` | `"sctp"` | `"tcp"` is non-standard and for lab use only. |
| `routing_context` | none | If set, ASPAC/DATA naming a different one is refused with ERR `0x19` before the model is consulted. |
| `network_appearance` | none | If set, DATA naming a different one is refused with ERR `0x15`. |

All three are read; all three are validated in `M3uaConfig::from_params` before anything binds,
with `?` rather than `unwrap()`, so a bad value fails startup naming the parameter.

## Verification

**No SCTP stack and no SIGTRAN peer exist on the development machine, so this has never spoken
to a real ASP or SGP.** That is the honest limitation and it is why the state is `Experimental`.

What was done instead, in `tests/server/m3ua/`:

1. **`codec_test.rs` — literal RFC octets, both directions.** Every expected vector is written
   by hand from RFC 4666 with the field decode in the comment, so it comes from the spec rather
   than from this implementation. Covers the padding rule at 1, 2 and 3 octets of padding, the
   Message Length's coverage of it, DATA's routing label, ERR, NTFY's packed Status field, ASPAC
   ACK's parameter order, and the receive direction on hand-written ASPUP and DATA messages
   NetGet did not encode. Plus the rejections: bad version, undersized and oversized lengths, a
   TLV shorter than its own header, a TLV running past the end, and a short Protocol Data.
2. **`transport_test.rs` — the refusal.** That SCTP is refused on macOS, that the refusal names
   SCTP / RFC 4666 / the escape hatch / its non-standardness, that a host *with* SCTP would get a
   real listener, that the lab transport binds, that its label warns, and that the declared
   default is `"sctp"`.
3. **`e2e_test.rs` — the whole path over the lab transport**, against a mocked model: an ASP
   comes up, is activated, exchanges an MSU with the point codes swapped and the SLS echoed, and
   BEAT and ASPIA are answered with **no** model call (which `verify_mocks` is what proves).
   Then the fail-closed pair: `wait_for_more` on an ASPUP produces ERR `0x0d` and leaves the ASP
   DOWN (and the following ASPAC earns ERR `0x06` with no second opinion), and a server with no
   policy at all refuses with `decision=no_policy_configured` in the log.

Not covered, and it is a long list: **SCTP itself** (never executed), interoperability with any
real implementation, multi-ASP behaviour, AS state, load sharing, routing key management,
SSNM, and anything that depends on SCTP's stream identifiers or multi-homing.
