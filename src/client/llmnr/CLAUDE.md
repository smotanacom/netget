# LLMNR Client (Querier) Implementation

LLMNR (RFC 4795) **querier**: asks the link to resolve a name, and reports *every* host that
answers. The responder half lives in `src/server/llmnr/` — read its `CLAUDE.md` too, especially
the header-flag table, because the two halves must agree bit for bit.

**State**: Experimental — see [Maturity](#maturity-why-experimental). **Privilege**:
`PrivilegeRequirement::None` — queries go out from an ephemeral port and responses are unicast,
so nothing binds 5355 and nothing needs a raw socket or a group join.
**Stack**: `ETH>IP>UDP>LLMNR`.

## The three things that make this not a DNS client

A DNS client sends a question to one resolver and reads one answer. None of that holds here, and
each difference is load-bearing in the code:

1. **A query has no single answer.** It goes to the whole link and *every* host claiming the name
   replies, unicast. Two hosts answering with different addresses is not an edge case: LLMNR
   authenticates nothing, so a host that answers faster than the real owner simply wins the name.
   So `run_query` collects for a whole window and reports **one `llmnr_response_received` event
   per responder**, plus `llmnr_conflicting_responses` when they disagree. It never takes the
   first answer.
2. **Nobody answering is the normal outcome.** RFC 4795 §2.1.1 forbids NXDOMAIN — a responder that
   does not own the name stays completely silent, so the host that *does* own it can still
   answer without racing a negative reply. An unanswered query is therefore the link saying "no
   host here claims that name", and it is modelled as its own event (`llmnr_query_timeout`) whose
   description tells the model in as many words that it is not a fault.
3. **The querier is the only thing that can reject a forgery.** There is no authentication of any
   kind. The random transaction ID and the echoed question are the entire defence.

### The socket is deliberately not `connect()`ed

`UdpSocket::connect` would filter datagrams to the one peer the query was sent to. On a multicast
query that peer is `224.0.0.252`, which never sends anything — every real answer comes from a
different host's unicast address. A connected socket would therefore discard **all** of them.
`send_to`/`recv_from` is not a stylistic choice here.

### Binding: `0.0.0.0`, not `127.0.0.1`

Measured on macOS 27 and recorded in `PROTOCOL_ROADMAP.md`, contradicting the usual assumption:
bound to `127.0.0.1` a socket **can** join a multicast group but **cannot send** to one —
`sendto` fails with `EADDRNOTAVAIL` (49), because loopback carries no multicast route. Bound to
`0.0.0.0` both work. The default bind follows the target's address family (`::` for IPv6).

Multicast TTL is set to 1 (RFC 4795 §2.5: an LLMNR query is link-scoped). The call is
best-effort; a failure is logged at DEBUG and does not stop a unicast query from working.

## Name canonicalisation — a real bug, worth not reintroducing

`Name::from_str("printer.local")` produces a **relative** name; a name decoded from the wire is
always **fully qualified**. Comparing the two is comparing different things, and the `name` field
the model sees would not match the responder half's (`printer.local` vs `printer.local.`). Every
query name is lowercased and `set_fqdn(true)` immediately after parsing, before it is used for
anything. That is the one place it happens.

## Header flags

Identical aliasing to the responder half, and for the same reason — `hickory-proto` has no LLMNR
mode:

| bit | DNS | LLMNR | this client |
|---|---|---|---|
| byte 2, bit 2 (`0x04`) | `AA` | `C` (Conflict) | cleared on queries; read off responses via `conflict_bit()` |
| byte 2, bit 0 (`0x01`) | `RD` | `T` (Tentative) | cleared on queries; read off responses via `tentative_bit()` |
| byte 3, bit 7 | `RA` | `Z` | always zero |

**The trap is the same one the responder documents, inverted.** A DNS client sets `RD` on every
query. Here that bit means "tentative", and a query carrying it is a different message. Both bits
are explicitly cleared in `run_query`, next to a comment saying why, because the surrounding code
looks exactly like a DNS builder.

## Response acceptance — `LlmnrClient::classify`

The one function that decides whether a datagram belongs to this query. `Err(reason)` is a
rejection carrying a human-readable reason, and the reason reaches the model in
`llmnr_query_timeout.discard_reasons` — so a refused forgery is visible rather than looking like
an unanswered query. Rejected, in order:

* anything `hickory-proto` cannot parse;
* `QR=0` (not a response);
* **transaction ID mismatch** — the response answers somebody else's question, or nobody's;
* `QDCOUNT != 1` — the question was not echoed;
* an echoed question whose **name**, **type** or **class** differs from what was asked;
* a response carrying no record of the queried type. A responder answers only for names it owns,
  so an answer-less response is a malformed claim, not a "no" — accepting it would let an empty
  packet count as a responder and manufacture a conflict.

A retransmission from a host already counted (same source *and* same data) is one responder, not
two. Counting it twice would fabricate the conflict this client exists to detect.

## What the model sees and controls

**Actions** — all three in `get_async_actions()`; `get_sync_actions()` is empty on purpose.
A client has one LLM entry point, which advertises the **union** of async, sync and the firing
event's actions, so the async/sync split cannot express a narrowing and duplicating the list into
both methods (which ~40 clients do) buys nothing.

| Action | Effect |
|---|---|
| `send_llmnr_query` | Resolve `name` (A/AAAA/PTR). A random transaction ID is generated per query. `target` overrides the destination for that one query |
| `wait_for_more` | Send nothing; end this resolution without disconnecting |
| `disconnect` | Close the socket and stop |

`target` exists because a multicast query is unobservable in a test on a loopback-only host (see
the binding note above). It is documented to the model as a testing override, not as a normal
knob.

**Events** — every one has a real emit site in `mod.rs`; `with_actions(...)` on all four, so the
model is never handed a question it has no vocabulary to answer.

| Event | When | Notable fields |
|---|---|---|
| `llmnr_connected` | socket bound | `target`, `local_addr`, `response_wait_secs` |
| `llmnr_response_received` | **once per responder** | `address`, `addresses`, `ttl`, `responder_address`, `responder_index`/`responder_count`, `conflict`, `tentative` |
| `llmnr_query_timeout` | window closed, nobody answered | `discarded_count`, `discard_reasons` |
| `llmnr_conflicting_responses` | ≥2 responders, ≥2 distinct answers | `answers[]`, `distinct_answers` |

No raw bytes or base64 anywhere: the model gets names, addresses and reasons, never a packet.

`llmnr_conflicting_responses` is additionally logged at WARN on **both** channels at its emit
site. The tracing line names each responder and what it claimed; the status-channel line carries
the name, the record type and the responder count, because that is the width the rail has.
`LogTemplate` has no warn level, so both are written explicitly in `report_outcome` rather than
in the template.

`llmnr_query_timeout`'s `discarded_count` is the true number of rejected datagrams, but
`discard_reasons` keeps only the first `MAX_RECORDED_DISCARDS` (10). Rejecting a datagram costs
no LLM call, so the collection loop processes as many as arrive — and the querier sits on an
ephemeral port on a link where the query it just sent was multicast to everyone. Uncapped, one
flood inside the response window would allocate a `String` and send an unbounded status message
per datagram, and then put the whole transcript into the model's prompt. The reasons repeat once
a flood is under way, so ten shows the pattern; the count is not softened.

## Startup parameters

| Parameter | Default | Effect |
|---|---|---|
| `bind_address` | `0.0.0.0` (`::` for an IPv6 target) | Local address to send from |
| `response_wait_secs` | 2 | How long each query collects responses |

Both are read in `mod.rs`, both through fallible accessors propagated with `?` — never
`unwrap()`, which over MCP kills the request task before it can report the error.

`response_wait_secs` is **not** a first-answer timeout. The window always runs to completion,
because a second responder arriving late is precisely the thing this client exists to see. That
is why the e2e tests raise it: a window that expires before a loaded machine's mocked model has
answered presents as "the querier ignored a valid response", which is a product bug, not a timing
one.

## Follow-ups: an iterative queue plus a depth cap

`run_actions` is an explicit work queue, not recursion. The DNS client used to await the model
and then call itself per follow-up action; because each level is a separately polled boxed
future, a non-converging model overflowed the stack and took the whole process down
(`IMPROVEMENTS.md` item 49). Stack depth here is constant however many rounds occur.

`MAX_FOLLOWUP_DEPTH` (6) bounds the *number* of rounds on top of that. The per-client LLM budget
would eventually stop a runaway, but not before it had flooded the link with queries — a bound on
model calls is not a bound on packets. Hitting the cap is logged at WARN on both channels.

**The model's answer is always executed.** `let _ = call_llm_for_client(...)`, counting
`.len()`, or logging the actions instead of running them is the single most common client defect
in this repo; `tests/client_event_wiring_test.rs` is the ratchet.

## Command channel

Adopted. `register_command_channel` is called **before** the connected-event LLM call, because a
dashboard-created client defaults to a `*` → manual routing rule and that call can park for
minutes waiting for a human — `[ send ]` has to work for the whole park.

An injected query reports `Executed { detail }` rather than `Sent { bytes_sent }`. The byte count
is truthful but useless; what the caller wants is who answered and whether they agreed, which is
what the detail string carries. The model is shown the result *after* the caller has been
answered, so a manual rule parked on `llmnr_response_received` cannot hold `[ send ]` open.

Every spawned task — the command loop and the conversation task — is registered with
`register_client_task`.

## Not implemented

* **TCP queries.** RFC 4795 §2.4 requires responders to listen on TCP (the responder half does)
  and says senders must support sending them. This querier is UDP only. It is the transport on
  which a non-zero RCODE is legal, so a TCP querier would also need to surface RCODEs, which
  nothing here models.
* **Retransmission.** RFC 4795 §2.7 has a querier retransmit after `LLMNR_TIMEOUT`. One
  transmission per action; the model can simply ask again.
* **The `C` bit on outgoing queries.** RFC 4795 lets a querier report a detected conflict back to
  the link. This client detects conflicts and tells the *model*; it does not assert them on the
  wire, because doing so is a claim about a name it does not own.
* **IPv6.** The code paths exist (`FF02::1:3`, `::` bind) and none of them has ever been run.
* **NBT-NS**, LLMNR's Windows companion. Separate protocol, separate port.

## Maturity: why Experimental

The bar for Beta is *works against real clients* — for a client protocol, against a real
independent **responder**. The evidence here fails on two axes at once:

* **Same project.** Three of four tests use NetGet's own LLMNR responder as the peer.
* **Same codec.** Both halves encode with `hickory-proto`, so the test asserts that one library
  round-trips through itself. This is the failure the root `CLAUDE.md` names for
  `webrtc_signaling`/`websocket`.

The real independent peers are the Windows resolver and systemd-resolved; neither exists on
macOS, and `resolvectl`/`systemd-resolve`/`avahi-resolve` are all absent here.

### `llmnr-poison` — the server half's note about it is wrong, and this matters

`src/server/llmnr/CLAUDE.md` records `llmnr-poison` as "a Responder-style *responder* — the same
role as this server, not a querier" and dismisses it. For the **server** that conclusion is right
(a responder cannot test a responder). For this **client** the role is exactly right, and the
crate is more usable than that note suggests:

* It **is a library** (crates.io `has_lib`, no binaries), v0.1.0, published 2026-08-13.
* Its only dependencies are `anyhow` and `tokio` — so it **hand-rolls the DNS wire format** and
  shares no codec with NetGet. Pointing this client at it would break the codec axis of the
  circularity outright.
* `pub fn llmnr_response(query: &[u8], spoof: Ipv4Addr) -> Option<(String, Vec<u8>)>` is a **pure
  function**: hand it the query bytes, get the name and the response bytes back. The test keeps
  its own ephemeral unicast socket, so none of the multicast/loopback problems apply.
* `pub async fn poison(spoof: Ipv4Addr) -> Result<()>` is **not** usable: it takes no bind
  address, claiming the fixed port 5355 plus NBT-NS 137 itself. A fixed port cannot survive a
  100-thread suite, and the group cannot be sent to on loopback anyway.

**No dependency was added** — that was out of scope for this work. The line it would need, under
`[dev-dependencies]`:

```toml
llmnr-poison = "0.1"   # independent (non-hickory) LLMNR response encoder for tests/client/llmnr
```

Two things to weigh before adding it: its GitHub repository (`icedracon/llmnr-poison`) currently
**404s**, and it is a brand-new single-author offensive-security crate, so it is an unvetted
supply-chain addition to the dev tree.

**Even wired up, this stays Experimental.** An independent response *encoder* is not a running
responder, and the same-project axis would still apply to three of the four tests. Beta needs a
real Windows or systemd-resolved peer completing a real exchange. What the current tests *do*
prove is worth having and is not nothing: transaction-ID and echoed-question matching both reject
a mismatch, multiplicity survives to the model, a conflict is raised, silence is reported as the
expected outcome it is, and every event's answer is executed.
