# DHCPv6 Server (RFC 8415)

**Status**: `Experimental` — see "Why not Beta" below, which is the most important section here.
**RFC**: RFC 8415 (DHCPv6), RFC 3646 (DNS options), RFC 3633 / RFC 8415 §21.21 (prefix delegation),
RFC 6355 (DUID-UUID).
**Transport**: UDP. Server port 547, clients on 546. IPv6 only — there is no IPv4 form.
**Privilege**: `PrivilegeRequirement::PrivilegedPort(547)`. The gate in `server_startup.rs` only
fires when the requested port is actually below 1024, so a test on an ephemeral port needs none.
**Binding**: `default_binding()` returns `BindingDefaults::port_based("::1", 547)` — loopback, like
every other protocol here. Pass `host: "::"` to serve a real link. An IPv4 host is refused at
`spawn()` with an error saying why, rather than binding a socket no DHCPv6 client can reach.
**Connectionless**: declared. Its "connections" are per-datagram bookkeeping entries that nothing
closes, so the 10-second idle sweep is correct for it.
**Feature**: `dhcpv6` (pulls `dhcproto`, already in the tree for `dhcp` and `bootp`).

## No storage, and here it matters more than usual

DHCPv6 is a *leasing* protocol, so the pull toward a lease table is strong. There is none, and
there must not be one. NetGet holds no bindings, no address pool, no record of what it said to
this client thirty seconds ago. The model answers every message from the event in front of it.

The consequence is worth stating plainly rather than hiding: **nothing prevents the model from
handing the same address to two clients**, or from confirming in a REPLY an address it never
advertised, or from renewing a lease that never existed. That is the model's job, not the
protocol's. If you want deterministic allocation, use a script handler and keep the pool in the
script — or in the sanctioned SQLite facility (`src/state/sqlite.rs`), which is a *runtime*
capability the operator opts into. Do not put a `HashMap<Duid, Ipv6Addr>` in this directory.

One piece of state does exist and is deliberately not a lease: `Dhcpv6RequestContext`, the values
a reply has to echo (transaction id, client DUID, IAIDs). A **fresh `Dhcpv6Protocol` is created
per datagram** and carries exactly that one message's context, so two clients whose LLM calls
overlap can never echo each other's transaction id. Copied from `src/server/dhcp/`, where the
shared-instance version of this was a real defect.

## LLM failure → silence, and why a Status Code is not the answer

**When the model cannot answer, this server sends nothing at all.** Not a Status Code, not an
empty REPLY, not a NoAddrsAvail. This is in the deliberately-silent class in the root `CLAUDE.md`
alongside its IPv4 sibling, and the reasoning is stronger here than for most:

- **A reply reconfigures the host.** It writes an address and its lifetimes, possibly a delegated
  prefix, and the client's recursive resolvers into the stack. A fabricated one misconfigures the
  machine or redirects its name resolution, and the client believes it for the whole valid
  lifetime — not for one request.
- **The protocol has no "ask again later".** RFC 8415 §21.13 defines the Status Code option as a
  statement about a *lease*: `NoAddrsAvail` means this link has no addresses to give,
  `NoBinding` means the client's lease does not exist here. Sending either because NetGet's
  backend is unreachable tells the client something false about its own configuration. Worse,
  a client that receives `NoBinding` in reply to a RENEW moves to REBIND and then gives the
  address up (§18.2.4) — a valid lease destroyed by an internal error.
- **Silence is the protocol's own retry path.** §15 has clients retransmit with exponential
  backoff, so a transient overload recovers on the client's own timer. That is what a 503 buys
  in HTTP; here it is free.
- **Nothing internal can leak.** The ~25-protocol "answering the peer is not a licence to tell it
  anything" defect cannot occur here, because nothing at all goes on the wire. No `WireFailure`
  string is ever written to a socket by this protocol.

The distinctions live in the **log** instead, with a stable `decision=` token per message — the
`src/server/radius/` convention:

| `decision=` | Meaning | Bytes sent |
|---|---|---|
| `model_reply` | the model's action produced a packet | yes |
| `model_reject` | the model answered and chose to send nothing (`no_response`) | no |
| `fail_closed_no_action` | the model produced no action at all | no |
| `fail_closed_llm_error` | the LLM call errored | no |

`grep decision=fail_closed_` finds every message NetGet dropped on the floor, distinct from the
ones the model deliberately declined to answer. The error itself is logged at ERROR with a
`category=overloaded` / `category=unavailable` tag from `WireFailure::classify` — DHCPv6 cannot
express that difference on the wire, so it is kept where an operator can act on it — and each
fail-closed message also logs a WARN noting the client will retransmit.

`tests/server/dhcpv6/e2e_test.rs::test_dhcpv6_llm_failure_sends_nothing` asserts the absence
directly, and asserts the mock rule was *called* so that the silence is provably the fail-closed
path rather than a datagram that never arrived.

## Messages, events and what is dropped

| Client message | Event | Normally answered with |
|---|---|---|
| SOLICIT (1) | `dhcpv6_solicit` | `send_dhcpv6_advertise`, or `send_dhcpv6_reply` with `rapid_commit` |
| REQUEST (3) | `dhcpv6_request` | `send_dhcpv6_reply` |
| RENEW (5) | `dhcpv6_renew` | `send_dhcpv6_reply` |
| REBIND (6) | `dhcpv6_rebind` | `send_dhcpv6_reply`, or `no_response` |
| RELEASE (8) | `dhcpv6_release` | `send_dhcpv6_reply` with a Success status code |
| INFORMATION-REQUEST (11) | `dhcpv6_information_request` | `send_dhcpv6_reply`, configuration only |

**Dropped before the model is consulted**, with a WARN naming the message type:

- **CONFIRM (4) and DECLINE (9)** — both are questions *about a binding*, and there are no
  bindings. RFC 8415 §18.3.3 is explicit that a server unable to perform the Confirm test "MUST
  NOT send a Reply", which is exactly NetGet's position. Asking the model would be asking it to
  invent an answer about state that does not exist.
- **RECONFIGURE (10), ADVERTISE (2), REPLY (7)** — server-to-client messages.
- **RELAY-FORW (12) / RELAY-REPL (13)** — relay envelopes this server does not unwrap.
- **Anything that does not decode.** A datagram that fails `v6::Message::decode` never becomes an
  event: there would be no transaction id to echo, so no reply could be built from the answer,
  and the round trip would be spent for nothing. This is the `dhcp` lesson applied up front.

Adding CONFIRM/DECLINE would mean adding events for them; the reason not to is the paragraph
above, not effort.

## Event payload

Every event carries: `transaction_id` (a number 0–16777215 — **three** octets, not four),
`client_duid` (structured, below), `requested_options` (the ORO decoded to option *names*, e.g.
`["DomainNameServers", "DomainSearchList"]`), `ia_id` / `ia_pd_id` (the client's IAIDs),
`client_addresses` / `client_prefixes` (what the client itself named — a hint in a SOLICIT, the
address it holds in a RENEW, the one it is giving up in a RELEASE), `source_address` and
`source_port`. `dhcpv6_solicit` additionally carries `rapid_commit`.

**No raw bytes anywhere.** Addresses and prefixes are IPv6 strings with numeric lifetimes; option
requests are names, not codes; the DUID is decoded.

### DUIDs are structured, not opaque

A DUID is the only identity DHCPv6 has — there is no `chaddr`. It is opaque *on the wire*, but
its first two octets name one of four documented shapes, so `describe_duid` publishes:

| field | always | meaning |
|---|---|---|
| `type` | yes | `"llt"`, `"en"`, `"ll"`, `"uuid"` or `"unknown"` |
| `text` | yes | canonical colon-hex, e.g. `00:03:00:01:00:11:22:33:44:55` — the stable key to match on |
| `hardware_type` + `hardware_type_name` | llt, ll | e.g. `1` / `"ethernet"` |
| `link_layer_address` | llt, ll | the MAC, colon-hex |
| `time` | llt | seconds since 2000-01-01 UTC |
| `enterprise_number`, `identifier` | en | `identifier` only when the bytes really are text |
| `uuid` | uuid | hyphenated form |

The reply actions take the same shape back in `server_duid`.

**`dhcproto::v6::duid::Duid` is not used to build these.** Its `link_layer` and
`link_layer_time` constructors take the link-layer address as an `Ipv6Addr` and therefore always
write **16** octets, so they cannot express a DUID-LL for a 6-octet Ethernet MAC — the common
case. All four forms are hand-encoded here instead, side by side, in `encode_duid`.

## Actions

`send_dhcpv6_advertise` (SOLICIT only) and `send_dhcpv6_reply` (everything else) share their
parameters: `addresses`, `prefixes`, `dns_servers`, `domain_search`, `t1`, `t2`, `status_code`,
`server_duid`, `ia_id`, `ia_pd_id`, `transaction_id`. Advertise adds `preference` (option 7);
reply adds `rapid_commit`. `no_response` sends nothing and is a real answer, distinct from a
failure.

Echoed automatically from the request context, so the model never has to supply them: the
transaction id, the Client Identifier, and the IA_NA / IA_PD IAIDs. The overrides exist for
replies built out of band (a script answering from stored state) and are what the E2E suite uses
to exercise the dynamic-echo path.

Validation that is deliberately strict rather than lenient, because a silently-dropped option
presents as a client timeout:

- `preferred_lifetime > valid_lifetime` is an **error**, not a clamp — RFC 8415 §21.6 has the
  client discard such an IA Address outright.
- `t1 > t2` (both non-zero) is an error, same reasoning.
- An unparseable entry in `dns_servers` fails the whole action rather than shipping a shorter
  resolver list than was asked for.
- `rapid_commit` on an ADVERTISE is an error: Rapid Commit *means* answering with a REPLY
  instead of an ADVERTISE, so the combination is incoherent.
- `addresses` with no IA_NA in the request (and no `ia_id` override) is an error naming that,
  rather than a packet the client will ignore.

### The default server DUID is constant on purpose

With no `server_duid`, replies carry DUID-EN with IANA's reserved documentation enterprise number
**32473** (RFC 5612 §5) and the identifier `netget`. A client sends its REQUEST to the Server
Identifier it saw in the ADVERTISE and rejects a REPLY carrying a different one — and NetGet
holds no state between those two model calls, so a randomly generated DUID would break every
four-message exchange. If you set `server_duid`, set it identically in both messages.

## Multicast

Real clients send to **FF02::1:2** (`All_DHCP_Relay_Agents_and_Servers`). The join is attempted
only when the server is bound to the unspecified address (`host: "::"`) — joining a group on a
loopback-only socket is meaningless and usually fails — using the `multicast_interface_index`
startup parameter (default 0, meaning the kernel chooses). **A failed join is logged and never
fatal**: unicast and relayed traffic still reach the server, which is what every test and every
relayed deployment uses. It has not been verified against a real multicasting client.

## Why not Beta

Beta means "works against real clients". No real DHCPv6 client can be pointed at this server:

- `dhclient -6`, `dhcpcd` and `odhcp6c` bind UDP/546, require root, and drive a kernel interface
  — none can be aimed at an ephemeral loopback port. **None of them is installed on this
  machine** (checked: `dhclient`, `dhcpcd`, `dhcp6c`, `odhcp6c`, `busybox` all absent).
- macOS ships no DHCPv6 client binary at all. `/usr/sbin/ipconfig` asks configd to run DHCPv6 on
  a real interface; it takes an interface name, not an address and port.
- There is no third-party Rust DHCPv6 *client* crate in the tree, and adding a dependency was out
  of scope for this work.

So the E2E peer is an **RFC 8415 encoder/decoder written in the test file** — an independent
reading of the spec, not an independent implementation. It deliberately does not use `dhcproto`,
because that is the codec this server encodes with, and a test that decodes with the same library
it encoded with asserts only that the library round-trips with itself. The root `CLAUDE.md` names
that failure mode (see the `rss` entry, which was Experimental for exactly this reason until an
independent parser replaced the circular one).

Promoting this to Beta means either an independent DHCPv6 codec crate on the test side, or a real
client driven on a real interface — the latter needing root and a network namespace, which is a
CI question rather than a code one.

## Known limitations

- No lease state of any kind (see above — this is the design, not a gap).
- CONFIRM, DECLINE, RECONFIGURE and relay messages are dropped (see above).
- **Per-IA status codes are not expressible.** `status_code` is written at the top level of the
  message, which is what RFC 8415 §18.3.2 calls for when the server has nothing for *any* IA. A
  status inside a specific IA_NA/IA_PD would need another parameter shape.
- The reply goes to the UDP source address of the request. A relayed exchange would need
  RELAY-REPL encapsulation back to the relay, which is not implemented.
- Temporary addresses (IA_TA, option 4) are neither reported nor offered.
- Authentication (option 11), `Reconfigure Accept` (20), server unicast (12), NTP (56) and vendor
  options (16/17) are ignored on input and cannot be sent.
- The multicast join is untested against a real client.

## References

- [RFC 8415 — DHCP for IPv6](https://datatracker.ietf.org/doc/html/rfc8415)
- [RFC 3646 — DNS configuration options for DHCPv6](https://datatracker.ietf.org/doc/html/rfc3646)
- [RFC 6355 — DUID-UUID](https://datatracker.ietf.org/doc/html/rfc6355)
- [RFC 5612 — enterprise number 32473 reserved for examples](https://datatracker.ietf.org/doc/html/rfc5612)
- [dhcproto](https://docs.rs/dhcproto/latest/dhcproto/) — server side only
