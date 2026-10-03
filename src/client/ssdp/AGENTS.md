# SSDP / UPnP discovery client

The **control point** half of SSDP: it sends `M-SEARCH * HTTP/1.1` and reads what answers.
The device half is `src/server/ssdp/` and is not this protocol's to modify.

Pointed at a real LAN this enumerates actual hardware — routers, TVs, printers, media
servers — and the model reads each responder's `SERVER`, `USN` and `LOCATION` and decides
what to search for next. That is the point of it; the loopback tests are scaffolding.

## Maturity: `Experimental`, and the reason is the peer, not the code

Every test peer is either NetGet's own SSDP server (same project, **same HTTPU codec**, written
in the same pass — the circular-evidence class the root `CLAUDE.md` names) or a device
hand-written inside the test from UDA 1.1 (an independent *reading* of the spec, not an
independent *implementation* — the `dhcp` / `usbip_client` class). Neither is a third-party
client, so neither can support a `Beta` rating.

**What would actually earn Beta**, in order of what this environment could realistically get:

1. **A real UPnP device on a real LAN.** Bind `0.0.0.0:1900`, `join_multicast: true`, search
   `ssdp:all`, and assert against a router or TV that NetGet did not write. This is the
   strongest evidence and needs nothing installed — only a network with a device on it and a
   human to run it. It cannot be automated in CI here.
2. **An installed UPnP device emulator** driven from the test. Nothing suitable is installed
   in this environment; `gupnp-tools`/`upnp-inspector` or a Python `upnpy`-style responder
   would do it, and would make the evidence reproducible.

**A note on what the server-side agent found, because it does not transfer.** That agent
established that *no Rust SSDP crate can be aimed at a unicast loopback port* — every one of
them multicasts to `239.255.255.250:1900` unconditionally. That finding is about crates that
**search**, i.e. control points, which is what would have tested the *server*. It cuts
differently here: what this client needs is a peer that **answers** — a device — and a device
implementation does not have the same constraint, because a device replies to whatever address
the search came from. So an emulator is a live option for this half even though a client crate
was not one for the other half. That is worth checking before repeating the conclusion.

Do **not** promote on the strength of `discovers_netgets_own_ssdp_server` passing. It shows the
two halves of NetGet agree; it cannot catch a mistake both halves make, and they share a codec
precisely so that they make the same ones.

## Shape: one-to-many, which nothing else in this tree is

Every other client here issues a request and reads *the* response. SSDP issues one datagram and
an unknown number of devices answer over the next `MX` seconds, each from its own address.

So a search opens a **collection window**:

- each responder raises its own `ssdp_search_response` as it arrives;
- when the window closes, one `ssdp_search_complete` reports how many answered and who they
  were.

Returning on the first datagram — correct for every other client here — would report one device
on a network of thirty. `tests/client/ssdp/e2e_test.rs::one_search_collects_every_responder_and_hears_an_announcement`
exists to fail if someone "simplifies" it back.

Window length is `MX` seconds by default, because that is what the devices were told to expect;
`response_window_ms` overrides it. A window shorter than MX drops exactly the devices whose
jitter landed late, which on a real network is most of them.

**Dedupe is keyed on `(source address, USN)`.** A device retransmitting is one device; a device
answering `ssdp:all` with several services is several genuine results. Keying on address alone
would collapse the second case, and keying on USN alone would merge two devices that share a
service type.

## Architecture: one task owns the search

Three things interleave — datagrams arriving, the window expiring, injected `[ send ]` commands
— and all three mutate the same search state. Rather than share it behind a mutex, `Session`
owns it outright and `tokio::select!`s over all three. A separate, minimal receive task does
nothing but `recv_from` and forward on an unbounded channel, so the socket always has a reader
even while the session is inside an LLM call, and no datagram is dropped for want of one.

The cost, stated plainly: an injected command waits behind an in-flight LLM call rather than
running concurrently (`udp` gives commands their own task for that reason). That is the right
trade here — a command that started a second search from another task would race the window
bookkeeping — and the command channel is bounded, so a command queues rather than being lost.
The channel is registered **before** the `ssdp_connected` LLM call, so a `manual` routing rule
that parks that call for a human does not make `[ send ]` read "no command channel" for the
whole park.

`connect()` returns as soon as the socket is bound and the tasks are spawned; the connected
event is raised inside the session task, so a parked connect event never delays client
creation. **Every** spawned task is registered with `AppState::register_client_task` — the
receive task and the session task — not just the read loop.

## Follow-up depth: bounded, and the bound is loud

Discovery is inherently recursive: search, see what answers, search again more specifically.
Nothing in that shape converges. A search started in reply to an event carries a depth;
`MAX_FOLLOWUP_DEPTH` (6) refuses to go deeper, with an ERROR on **both** log channels naming
the limit. Refusing silently would be the same defect as discarding the model's answer.

**There is no `Box::pin` here and that is deliberate**, because the usual fix for this shape is
a boxed recursive call. The cycle already passes through a queue: `send_msearch` only
*enqueues* a search, the session loop starts it, the socket delivers answers later, and the
events those raise are handled on a later turn of the same loop. No `async fn` awaits itself,
so there is no infinitely-sized future (E0391) to box — the situation the root `CLAUDE.md`
describes for `datalink`'s pcap loop. Queuing rather than replacing also means a `send_msearch`
returned in reply to an `ssdp_search_response` does not truncate the window it was answering
inside.

The model's answer is **never** discarded: `call_model` returns `Option`, `None` means only
"no instruction configured, or the call failed", and every `Some` is executed by its caller.

## LOCATION is not fetched, on purpose

Every responder carries a `LOCATION` URL for its device-description XML, and fetching it is the
obvious next step. NetGet does not do it, and there is no action for it.

Two reasons, and the second is the one that decides it:

1. **It is HTTP, not SSDP.** This repo has an HTTP client; a second, worse one hidden inside a
   discovery protocol is exactly the per-protocol sprawl the decentralization rule exists to
   prevent.
2. **An automatic fetch turns discovery into an outbound request the operator never asked
   for.** A control point that scans a network and then connects to every URL any device
   advertises is a different tool with a different risk profile, and the `LOCATION` is
   attacker-controlled input — anything on the segment can put any URL in front of it.

The URL is surfaced in the event instead, so the model can say what it found and the operator
can open an HTTP client against it deliberately. If a future pass adds fetching, make it an
explicit action the model chooses per URL, never a side effect of a search.

## Multicast, and the trap that is the opposite of the usual one

Measured on macOS 27, and it contradicts the received wisdom that the *join* is what fails:

> Bound to `127.0.0.1`, `join_multicast_v4(239.255.255.250)` **succeeds**; `sendto` to the
> group fails with **`EADDRNOTAVAIL` (49)**, because loopback carries no multicast route.
> Bound to `0.0.0.0` both work.

So: do not "fix" a join that is not broken. `send_failure_hint` turns that bare OS message into
one naming the cause and both ways out (`bind_address: "0.0.0.0"`, or a unicast `target`),
because left raw it reads as a bug in NetGet.

The failure also reaches the **model**, not only the log: a send failure raises
`ssdp_search_complete` with `send_error` set and no responders. Only the model can choose a
different target, so telling only the operator would leave it waiting on a window that never
opens. `send_error` is declared as a (non-required) parameter of that event, so the model's
schema mentions it — it used to be put in the payload and left out of the declaration, which
made the one field explaining a zero-responder search invisible to the reader who needed it.

Two more things worth knowing about the multicast path:

- **Joining the group is not enough to hear announcements.** Devices multicast `NOTIFY` to
  `239.255.255.250:1900`; a socket on an ephemeral port is in the group and will never be
  delivered any. `local_port: 1900` is required, and the code warns explicitly when the join
  succeeded but the port makes it useless — a silent version of that presents as "no device
  ever announces itself", which is indistinguishable from a broken client.
- **The join is best effort.** Replies to our own M-SEARCH come back unicast, so searching does
  not depend on it, and refusing to start would make the client useless for the local testing
  it is most used for.

## Deliberate refusals

- **CR/LF in `st` is refused, not sanitised.** It would end the `ST` line early and turn the
  rest of the model's string into further headers of *our* request — response splitting,
  pointed at discovery. Shares `server/ssdp/message.rs::validate_header_piece` with the device
  half.
- **`MX` is clamped, not refused.** UDA 1.1 §1.3.2 requires a *device* to treat MX > 5 as 5, so
  an oversized value is pointless rather than wrong; failing the search over a detail the
  device ignores would lose a discovery for nothing. `MX: 0` becomes 1 — zero asks every device
  on the network to answer at once.
- **`target` is parsed in `execute_action`**, so a typo is reported to the model as a rejected
  action instead of becoming a search that silently goes nowhere.
- **An inbound `M-SEARCH` is ignored.** We are a control point, not a device; answering would
  be a lie, and answering our own group traffic would loop.
- **A non-200 status line is ignored.** UDA 1.1 §1.3.3 makes the answer a 200; treating
  anything else as a discovery would put a device that refused us into the results.

## Sharing the codec with the server half

`message::parse`, `HttpuMessage`, `validate_header_piece`, `SSDP_GROUP_V4`, `SSDP_PORT` and
`MAX_MX_SECONDS` come from `src/server/ssdp/message.rs`. SSDP's grammar is a start line plus
`NAME: value` lines; a second hand-rolled parser would drift from the first the moment either
was touched. The decentralization rule is about not putting per-protocol logic in central
files, not about duplicating a protocol's own codec between its two halves.

`render_msearch` is here rather than there because it is the control point's message and
`server/ssdp/` is not this protocol's to edit. It and `max_age_of` are `pub` because all tests
live in `tests/` and anything a test asserts on has to be visible.

## Client-side conventions this follows

- `get_sync_actions()` is **empty**. A client has one LLM entry point
  (`call_llm_for_client` serves both the initial instruction and every network event), so no
  client can express a narrowing and the async/sync split is vestigial. `client_llm_action_set`
  unions async ∪ sync ∪ the event's own actions; duplicating the list into both methods, as ~40
  clients do, works around a bug that no longer exists.
- Every event attaches the full action set via `with_actions(...)`.
- No raw bytes and no base64 anywhere in an action parameter or event payload: headers reach
  the model as a name→value map, and `max-age` is handed over already parsed as a number.
