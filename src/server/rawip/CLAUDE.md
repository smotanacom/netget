# Raw IP protocol-N — the generic home for IP protocols that have no other

**Status**: `Experimental`
**Feature**: `rawip` (deps: `pnet`, `socket2/all`)
**Privilege**: `PrivilegeRequirement::RawSockets` — `CAP_NET_RAW` on Linux, root elsewhere
**Metadata**: declares `.connectionless()`
**Registry name**: `Raw IP`

This server binds **one `SOCK_RAW` socket for one operator-chosen IP protocol number**, decodes
the **IP header**, surfaces the payload opaquely, and lets the model decide. GRE (47), ESP (50),
AH (51), SCTP (132) — and any number IANA has assigned or not — are all the same server started
with a different `protocol_number`.

## The rule that defines this module

**There is no per-protocol branching here, and there must never be.**

No `match protocol_number { 47 => parse_gre(), 50 => parse_esp(), … }`. The root `CLAUDE.md`'s
decentralization rule exists precisely to stop that: the moment this file knows about GRE
specifically it has become the wrong thing, the next change adds ESP, then L2TP, and it is the
centralized per-protocol logic the architecture forbids. **A protocol that deserves real parsing
deserves its own module under `src/server/`.** That is not a deferral — it is the correct
outcome, and this server existing does not make it less so.

What is generic, and therefore what this module actually implements:

* the **IP header** — the same twenty bytes (IPv4) or forty bytes (IPv6) whatever rides on top;
* the **payload boundary** — where the header ends and the opaque bytes begin;
* the **event and action shape** — one event, two actions, identical for every number.

The one table keyed by protocol number is `ip_protocol_name()`. It maps a number to its IANA
*name*, so the model reads "47 (GRE)" instead of "47". **A name is not behaviour**: nothing
branches on the result, an unknown number is reported as a number, and adding an entry cannot
change what the server does. `decoding_does_not_depend_on_the_protocol_number` in the test suite
asserts this directly — decode the same packet with twelve different protocol numbers and every
field, including the payload slice, must come out identical.

## Layout

`mod.rs` is deliberately in two halves.

**The decoder** (`decode_ip_packet`, `Ipv4Header`, `Ipv6Header`, `ip_protocol_name`,
`encode_payload_for_event`, `decode_payload`, `packet_event_data`) is pure: functions over a byte
slice, no I/O, no state, no privilege. That is what makes it testable against literal packet
bytes, and it is the only part of this protocol that is actually proven.

**The transport** (`RawIpServer`) opens the socket and runs the receive loop. It **has never been
executed** — see Maturity below.

`actions.rs` holds the metadata, the one event and the two actions.

## Startup parameters

Three declared, three read (all in `RawIpConfig::from_startup_params`, which is the only place
any of them is consulted).

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `protocol_number` | **yes** | — | IP protocol number, 0-255. The only thing that makes the server specific. |
| `ip_version` | no | `"ipv4"` | `"ipv4"` or `"ipv6"`. Selects the socket domain and the header format. |
| `transport` | no | `"raw"` | `"raw"` = real `SOCK_RAW`. `"udp"` = the unprivileged **test** transport. |

`protocol_number` has **no default**, and that is correct: a generic protocol has nothing to fall
back on. Starting without it is an error naming the parameter.

**6 (TCP) and 17 (UDP) are refused**, with a message pointing at the `tcp` and `udp` protocols.
Those have real implementations, and a `SOCK_RAW` listener on either would compete with the
kernel's own stack for the same packets while being unable to complete a session.

Everything goes through `?`. `StartupParams` accessors are never `unwrap()`ed: these values come
from the model or an MCP client, and a panic here would kill the per-request task before it could
reply (root `CLAUDE.md`, startup parameters).

## The event

`rawip_packet_received`, raised for **every packet whose IP header decodes**. A packet that does
not decode is dropped and logged, never surfaced: the event's contract is that its header fields
are real, and handing the model an undecodable blob under field names it would then reason about
is worse than dropping it.

Common fields:

| Field | Meaning |
|---|---|
| `ip_version` | 4 or 6 |
| `source` / `destination` | addresses, as strings |
| `protocol` / `protocol_name` | the upper-layer number, and its IANA name or `null` |
| `payload_length` | how many payload bytes were **captured** |
| `payload` | the payload, in the encoding named below |
| `payload_encoding` | `"utf8"` or `"hex"` — stated, never left to be guessed |
| `payload_truncated` | true when more than `EVENT_PAYLOAD_LIMIT` (2048) bytes were captured |
| `payload_incomplete` | true when the header promised more payload than arrived |
| `listening_protocol_number` | what this server is bound to |
| `connection_id` | the per-packet bookkeeping entry |

IPv4 adds `ihl`, `header_length`, `dscp`, `ecn`, `total_length`, `identification`,
`flags` (`{reserved, dont_fragment, more_fragments}`), `fragment_offset`, `fragmented`, `ttl`,
`header_checksum`, `options_present`, `options_length`.

IPv6 adds `traffic_class`, `flow_label`, `ipv6_payload_length`, `next_header`,
`next_header_name`, `hop_limit`.

> **`payload_length` vs `ipv6_payload_length`.** The first is how many bytes were actually
> captured after the header, in both versions. The second is the IPv6 header's own field. They
> differ whenever a packet is cut short, which is exactly when the model needs to know.

The header checksum is reported and **not validated**: a raw socket has already had the kernel
check it, and re-checking would only reject packets the OS accepted.

**IPv6 extension headers are not walked.** `next_header` is reported as it appears; if it names
an extension header rather than an upper-layer protocol, the payload begins with that extension
header and the model is looking at it. Walking the chain means knowing which numbers are
extension headers and how each is sized — per-protocol knowledge this module deliberately does
not hold.

## Actions

**`send_rawip_packet`** — `destination` (required), `payload` (required), `encoding`
(`"utf8"` default / `"hex"`), `ttl` (0-255; the IPv4 TTL and the IPv6 hop limit are the same
field under two names, so there is one parameter, not two).

**`no_response`** — explicit silence. A *decision*, logged as `decision=model_reject`, which is
what makes it different from answering nothing at all.

### The payload is genuinely opaque, and that is the one legitimate encoded field

The root `CLAUDE.md` forbids raw bytes and base64 in action parameters because models cannot
produce or parse them reliably, and structured fields are almost always available instead. Here
they are not: netget does not implement the protocol above IP, by construction. So the payload is
bytes, and the rule that applies is the **other** one:

> **If an action documents an encoded field, the executor must actually decode it.**

`RawIpProtocol::execute_send_packet` does. `"48656c6c6f"` with `encoding: "hex"` becomes the five
bytes of `Hello`; the same string with `encoding: "utf8"` becomes its own ten ASCII bytes. The
encoding is **never sniffed** — that string is simultaneously valid text and valid hex, and only
the sender knows which it meant. An unknown encoding is an error, not a fallback to text: quietly
treating `"base64"` as utf8 would put the base64 characters on the wire.

The reference defect is `send_tcp_data`, which was documented in three places as accepting hex
and whose executor called `as_bytes()`, so a model following the documentation put literal ASCII
on the wire. `the_executor_actually_decodes_the_encoding_it_documents` and the end-to-end
`the_full_path_runs_and_a_hex_payload_arrives_as_bytes` both exist to stop that returning.

Inbound is symmetric: the event **states** `payload_encoding` rather than leaving the model to
infer it, so echoing the same convention back round-trips.

Payloads are truncated for logging with `crate::utils::truncate_for_log`, never by byte-index
slicing — that panicked this codebase on multi-byte UTF-8.

## LLM failure → silence, and it is not a compromise

When the LLM call fails, **nothing goes on the wire**.

This is not "we couldn't think of a good error to send". A generic IP protocol has **no error
frame**, because netget deliberately does not implement the protocol above IP — that is the whole
reason this server exists. Any bytes emitted would be a guess at a format nobody has defined, and
a guess a peer parses is worse than a packet it never receives.

In particular **no `WireFailure` text reaches the wire**. There is no wire format to carry it.
`WireFailure::classify` is still called, but only to tag the log line.

The wire cannot carry the distinction between the outcomes, so the log must. Every packet ends
with one stable `decision=` token, following `src/server/radius/`:

| Tag | Meaning |
|---|---|
| `model_reply` | a packet was actually emitted |
| `model_reject` | the model answered `no_response` — deliberate silence, a real decision |
| `model_silent` | the model answered nothing at all |
| `fail_closed_action_error` | every action it produced failed to execute |
| `fail_closed_no_reply` | actions ran but none produced a packet |
| `fail_closed_llm_error` | the LLM call itself errored; carries `category=overloaded\|unavailable` |

Grep `decision=fail_closed_` for every packet netget failed to answer, as distinct from one it
deliberately did not answer.

## Startup reports failure

`spawn()` creates both raw sockets **before** any task is spawned, so a privilege failure
propagates out of `Server::spawn`, `server_startup` records `ServerStatus::Error`, and an MCP
caller sees the reason. This is the ARP/DataLink/ICMP defect the root `CLAUDE.md` records — a
server sitting in `Running` having captured nothing is worse than one that refuses to start — and
it is avoided by construction here rather than patched in.

## The UDP test transport, and what it does not buy you

`transport: "udp"` binds a UDP socket and reads **each datagram as a whole IP packet**: the same
decoder, the same event, the same executor and the same emit path, with no privilege. It exists
so the decode → event → LLM → action path can be tested at all. Replies go back to the peer that
sent the datagram; `destination` is logged but cannot be honoured, and `ttl` is not applied
(there is no IP header to put it in).

**Never use it in production.** Nothing on a real network sends IP packets inside UDP datagrams.

> **It does not make the protocol startable unprivileged through the normal path.** The privilege
> gate in `server_startup` reads the protocol's *static* `metadata()`, which declares
> `RawSockets`, and `metadata()` cannot see startup parameters. So `ServerForm::create` with
> `transport: "udp"` is still refused on an unprivileged machine. The tests reach the transport by
> calling `RawIpServer::spawn_with_llm_actions` directly. If that ever needs to change, the honest
> fix is a privilege requirement that can vary with configuration, not a metadata that lies.

## Maturity: `Experimental`, precisely

**Proven** (see `tests/server/rawip/CLAUDE.md`):

* the IPv4 and IPv6 header decoders, against literal RFC 791 / RFC 8200 bytes — with options,
  without options, fragmented, and cut short;
* that truncated and malformed input is **refused rather than panicking** — this decoder is the
  only thing between a stranger's bytes and the receive loop;
* that decoding does not depend on the protocol number (the genericity claim itself);
* the executor's hex decoding, in both directions;
* the deliberate silence on LLM failure, and on `no_response`.

**Not proven, and not to be claimed:**

* **the raw socket transport, which has never been executed.** Opening `SOCK_RAW` needs root or
  `CAP_NET_RAW`; nothing in this repo has bound one, sent on one, or received on one.
* outgoing TTL / hop-limit handling (`set_ttl`, `set_unicast_hops_v6`) — never called for real.
* the kernel's own IP header construction on send. Without `IP_HDRINCL` the kernel builds it; what
  it puts in the source address field on a multi-homed host is unobserved.
* **IPv6 raw sockets receive no IP header.** The kernel strips it, so on that path the decoder
  would see only the payload, fail to decode, and the packet would be dropped and logged. This is
  a known gap, stated rather than papered over; the IPv6 decoder is proven against literal bytes
  and is directly useful on the UDP transport and to any future caller that has whole packets.
* BSD `total_length` byte order. Historically, BSD raw sockets delivered `ip_len` in *host* byte
  order. The decoder reads it big-endian per RFC 791 and treats it only as an upper bound that
  must be self-consistent, so a host-order value degrades to "use everything captured" rather
  than producing a wrong slice — but this has not been observed on a real macOS raw socket.

**Path to Beta**: run it under `sudo` against a real independent peer — GRE (47) from a Linux
`ip tunnel` endpoint is the obvious case — and assert both directions. Nothing less counts; the
root `CLAUDE.md` records three protocols that held a higher rating on evidence that was never a
real client.

## Things that are deliberately absent

* **No storage.** Nothing here persists anything. The model tracks whatever state it needs in its
  own memory, or opens the generic SQLite facility.
* **No reassembly.** Fragments are reported as fragments (`fragmented`, `fragment_offset`,
  `more_fragments`) and handed over individually. Reassembly is stateful, and doing it right is a
  protocol of its own.
* **No checksum validation or generation.** Inbound the kernel has done it; outbound the kernel
  does it.
* **No async actions.** Every verb needs the running server's socket, which the stateless registry
  object cannot reach — an async action would execute and put nothing on the wire, which is worse
  than not offering it.
