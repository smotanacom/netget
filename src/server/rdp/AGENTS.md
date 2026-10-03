# RDP Server — connection-negotiation slice

**Status**: `DevelopmentState::Experimental`. Default port 3389 is unprivileged, so
`privilege_requirement` is `None`.

## What this implements, and exactly where it stops

RDP is enormous. This implements the **first exchange only**: the TPKT-framed X.224 Connection
Request / Connection Confirm carrying the RDP negotiation, [MS-RDPBCGR] 2.2.1.1 and 2.2.1.2.

The server:

1. Reads a client's TPKT + X.224 **Connection Request** and parses:
   - the `Cookie: mstshash=<user>\r\n` routing token (the username the client presents), and
   - the **RDP_NEG_REQ** (`requestedProtocols` bitmask + flags). No RDP_NEG_REQ means the client
     asks for standard RDP security, reported as `["RDP"]`.
2. Raises `rdp_connection_request` with `{cookie_username, requested_protocols, requested_protocols_flags}`.
3. Writes the **Connection Confirm** the model chose:
   - `send_rdp_negotiation_response` → X.224 CC with **RDP_NEG_RSP** selecting one protocol, or
   - `reject_rdp_connection` → X.224 CC with **RDP_NEG_FAILURE**.
4. Half-closes the connection.

**It stops there.** There is **no MCS/GCC exchange, no security exchange (TLS/CredSSP/RC4), no
capability exchange, and no bitmap output.** No desktop frame is rendered and a real client does
not reach a session. This is deliberately a smaller, *correct and testable* slice rather than a
large, blind, unverifiable one — see "Honesty about testing" below. A client completing the
negotiation is real progress against the documented handshake entry point, and nothing beyond
that is claimed in `metadata().notes`.

### Why negotiation is a real LLM surface

The model decides the server's security posture per connection: select the protocol the client
offered (`TLS`, `HYBRID`/NLA, standard `RDP`, …), or **reject** and demand one it did not
(`SSL_REQUIRED_BY_SERVER`, `HYBRID_REQUIRED_BY_SERVER`, …). That is a genuine decision, not a
mechanical echo.

## Hand-written, no crate

Like the VNC server, this hand-rolls the wire format and pulls in **no new dependency**. `ironrdp`
is the main Rust RDP crate, but it was neither vendored in this build nor needed for this slice —
the X.224 negotiation is a fixed 19-byte Connection Confirm and a short, length-bounded parse of
the Connection Request. Byte builders and the name↔value maps live in `actions.rs`; the socket
loop and the CR parser live in `mod.rs`.

## Wire layout (the exact bytes)

Connection Confirm this server emits — 19 bytes total:

```
03 00 00 13            TPKT: version=3, reserved=0, length=0x0013 (19)
0E                     X.224 LI = 14
D0                     X.224 Connection Confirm (CC)
00 00                  DST-REF
00 00                  SRC-REF
00                     class option
02|03                  RDP_NEG_RSP (0x02) or RDP_NEG_FAILURE (0x03)
<flags>                1 byte (RSP: EXTENDED_CLIENT_DATA_SUPPORTED=0x01; FAILURE: 0)
08 00                  length = 8 (little-endian)
<u32 LE>               selectedProtocol (RSP) or failureCode (FAILURE)
```

Protocol values: RDP=0, TLS/SSL=1, HYBRID/CredSSP/NLA=2, RDSTLS=4, HYBRID_EX=8, RDSAAD=16.
Failure codes: SSL_REQUIRED_BY_SERVER=1, SSL_NOT_ALLOWED_BY_SERVER=2, SSL_CERT_NOT_ON_SERVER=3,
INCONSISTENT_FLAGS=4, HYBRID_REQUIRED_BY_SERVER=5, SSL_WITH_USER_AUTH_REQUIRED_BY_SERVER=6.

## Event and actions

One event, emitted once per connection, with actions attached:

| Event | Actions |
|---|---|
| `rdp_connection_request` | `send_rdp_negotiation_response`, `reject_rdp_connection` |

Parameters are **structured** (a `selected_protocol` / `failure_code` *name*, never raw bytes or
a bitmask the model has to compute). The executor maps the name to the value and builds the
literal frame; a name it does not recognise is a clean error, not a wrong frame on the wire.

There are no async actions.

## Dashboard injection (`[ message this peer ]` / `[ disconnect this peer ]`)

Every connection registers a peer handle (`server::peer_support`) right after it is tracked and
*before* the blocking Connection-Request read, so a client that connects but has not yet sent its
CR — or an event parked by a manual `*` rule — is still reachable by the operator.
`AppState::send_to_peer` runs the injected action through the same executor as the LLM path, so an
injected `send_rdp_negotiation_response` / `reject_rdp_connection` is encoded by exactly the model's
code and both return `ActionResult::Output` — **there is no `Custom`-result gap**. `close_connection`
gained an explicit arm in `execute_action` (not offered to the model — the slice always answers with
a CC and half-closes on its own) so `[ disconnect this peer ]` half-closes the write side and the
client reads EOF. The handle is removed on every exit path through the single cleanup in
`handle_connection`.

Connection counters are live: `negotiate` calls `update_connection_stats` for the CR read
(`bytes_received`) and the CC write (`bytes_sent`), so the rail shows real `↓ ↑` rather than `↓0 ↑0`
and `last_activity` advances. (Injected peer writes go through `peer_support`, which does not itself
update counters — only the protocol's own reads/writes are counted.)

Test: `tests/server/rdp/peer_inject_test.rs` — zero LLM calls (parked-read injection + a `*` static
handler for the counter path).

## Fail-closed behaviour

The most dangerous default here would be to accept standard RDP when the model says nothing —
that is the OAuth2 fail-open shape. Instead:

- **LLM error, or no usable action** → the server itself emits an **RDP_NEG_FAILURE** with
  `SSL_REQUIRED_BY_SERVER` and closes, logging a WARN. This path is in `mod.rs`, not chosen by
  the model.
- **Model rejection** (`reject_rdp_connection`) is structurally distinct: it carries the model's
  own chosen failure code and is logged as a deliberate rejection.

A silent "accept anything" was never an option.

### …and the wire cannot tell the two apart, so the log must

`build_negotiation_failure(SSL_REQUIRED_BY_SERVER)` produces **byte-identical** output whether
the model chose it or netget fell back to it. There is no free-text field in an X.224 Connection
Confirm — no place a `WireFailure` category could go — and inventing a `failureCode` to mean "the
backend is busy" would tell the client something [MS-RDPBCGR] does not define. So every outcome
carries a `decision=` tag, the shape `src/server/radius/` establishes for exactly this case:

| Outcome | `decision=` |
|---|---|
| Model selected a protocol (RDP_NEG_RSP on the wire) | `model_accept` |
| Model refused (RDP_NEG_FAILURE on the wire) | `model_reject` |
| Model answered with no usable action | `fail_closed_model_silent` |
| An action came back and the executor refused it | `fail_closed_action_error` |
| Backend failure | `fail_closed_llm_error` (with the `WireFailure` category alongside) |

Accept vs reject is read off the **RDP_NEG type octet of the bytes the executor produced**
(`negotiation_kind`, offset 11), not off the action name the model used: the executor is what
decides what goes on the wire, and only its output knows what that was.

## Input safety

- The TPKT length field is client-controlled; it is bounded (`11..=MAX_X224_LEN`, 2 KiB) **before**
  the X.224 buffer is allocated.
- Every field is length-checked before indexing; a short or non-CR TPDU is a clean error that
  closes the connection. No `unwrap()` on anything parsed from the network.
- The routing cookie is decoded with `from_utf8_lossy` (cannot fail).

## Honesty about testing

No real RDP client (`xfreerdp`/FreeRDP or Microsoft `mstsc`) was installable on this macOS host
(`brew install freerdp` was unavailable at the time of writing), so **the handshake was not
driven by a real client**. Correctness is therefore pinned against **[MS-RDPBCGR]-derived literal
bytes**: the E2E suite sends a real, correctly-framed X.224 Connection Request (with and without
an RDP_NEG_REQ, and with an `mstshash` cookie) and asserts the exact 19 bytes of the Connection
Confirm, plus the parsed event fields. This is weaker evidence than a real client rendering a
frame, and it is stated as such here and in `metadata().notes`.

To go further (a client reaching a session) requires MCS Connect-Initial/Response (BER + GCC
PER), the security exchange, Client/Server capability sets, and bitmap encoding — none of which
this slice attempts.

## Manual verification (if a client is installed)

```bash
./cargo-isolated.sh run --no-default-features --features rdp --release
# prompt: "RDP server on port 3389 that selects TLS during negotiation"
xfreerdp /v:127.0.0.1:3389 /sec:tls    # completes negotiation, then the connection ends
```

Expect the client to log a successful negotiation (selected protocol) followed by a disconnect,
because the slice ends after the Connection Confirm.

## References

- [MS-RDPBCGR] 2.2.1.1 Client X.224 Connection Request PDU
- [MS-RDPBCGR] 2.2.1.2 Server X.224 Connection Confirm PDU
- RFC 1006 (TPKT), ITU-T X.224 (COTP)

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and sent no Connection Request held a socket, a task and an `AppState` entry
forever. It now declares both halves; the constants and the reasoning live beside them in
`src/server/rdp/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 30s | RDP is client-speaks-first: the X.224 Connection Request is the very first thing on the wire, sent by `mstsc`, FreeRDP or rdesktop from inside their own connect path with no user interaction in between. A peer that has sent nothing has negotiated nothing. |
| `IN_FRAME_READ_TIMEOUT` | 15s | A narrower claim than the first bound, so a shorter number: these bytes belong to a TPKT frame whose header has already arrived, and a Connection Request is at most `MAX_X224_LEN` bytes that a real client writes in one go. Nothing legitimate pauses in the middle of it. This server's whole session is one request and one reply, so there is no third case — the model round-trip happens after the request has been read in full. |
| `MAX_CONNECTIONS` | 256 | Refusal: **nothing**. RDP's own refusal is a Connection Confirm carrying `RDP_NEG_FAILURE`, and it is a *reply*: a capped peer is refused before its Connection Request has been read, so there is no `srcRef` to answer and no requested protocol to fail. A fabricated Connection Confirm would also be a positive assertion — it tells the client which security protocol to speak next — which is what a refusal must not do. `accept_bounded` logs it at WARN with `decision=fail_closed_connection_cap`. |

**There is no NetGet RDP client**, so this bound has no peer of ours to strand — the fourth
exemption in `PROTOCOL_QUALITY.md`'s three-state test. Note that the greeting test does not
apply here: RDP is client-speaks-first.

**The deadlines are applied with `IdleTimeoutReader`, not a `tokio::time::timeout` around one
call**, because the Connection Request is read in two steps — the TPKT header, then the X.224
body whose length that header declares — and a peer that sends `0x03` and stalls would otherwise
be unbounded. Its deadline is armed lazily, only while a read is pending, so the model
round-trip, and a `manual` rule parking the negotiation for a human
(`src/state/intercepts.rs`, 300s by default), run with no clock against them.

`tests/server/rdp/connection_bounds_test.rs` drives all three from the wire: a silent peer is
closed at the first bound, a connection whose answer is parked for a human is not closed at all,
and the connection past `MAX_CONNECTIONS` is answered with the refusal above and then a clean
EOF. Each was verified by removing the thing it tests — the deadline, the busy marking, the cap —
and watching it fail. `tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound
is removed from the source, and `tests/accept_bounded_test.rs` covers the shared cap mechanism
itself, including that a busy connection is never reported as idle.
