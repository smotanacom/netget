# WS-Discovery server (target service)

NetGet as one or more WS-Discovery target services: SOAP-over-UDP on 3702, answering Probe
and Resolve and sending Hello/Bye. What an ONVIF camera, a WSD printer or a Windows host
(wsdd on Linux) does on the network. Hand-written codec in `wire.rs`, shared with the client
(`src/client/wsdiscovery/`); XML is read with quick-xml and written as text.

## Versions

Both: WS-Discovery 2005/04 (what Windows, ONVIF and wsdd speak, with WS-Addressing 2004/08)
and the OASIS 1.1 2009/01 namespaces (WS-Addressing 1.0). An answer is written in the version
the request came in. `Version::parse` takes `2005/04` or `2009/01`.

## Types and scopes

Types are QNames, carried in events and actions as Clark notation (`{namespace}Local`). The
parser resolves prefixes through the message's own `xmlns` declarations. Four conventional
prefixes are also accepted as input (`wsdp`, `pub`, `dn`, `tds`, in `wire::WELL_KNOWN`), and
**on the wire NetGet writes those namespaces with exactly those prefixes**. That is not
cosmetic. wsdd compares the Types text literally and answers `wsdp:Device` and nothing else,
whatever the prefix is bound to. Other namespaces get `n0`, `n1`, ….

Scope matching is the model's decision: the event carries `scopes` and `match_by` as sent, and
the instruction says what the services' scopes are.

## Who answers

- `wsd_probe` → `wsd_probe_match{matches:[…]}`, or nothing.
- `wsd_resolve` → `wsd_resolve_match{endpoint_reference, …}`, or nothing.
- `wsd_announcement` (a Hello/Bye heard from someone else) → optionally `wsd_send_hello` /
  `wsd_send_bye`.

`WsDiscoveryProtocol::for_request` carries the request's MessageID (for RelatesTo), version and
the AppSequence counter, so an answer is bound to its request. The registry instance only
validates shape and returns `NoAction`.

Matches go unicast to the sender's address. Announcements go to `announce_target`, which
defaults to the group (`239.255.255.250:3702`).

**Deliberately silent.** WS-Discovery has no negative reply: a probe nobody matches is answered
by silence. So a backend failure sends nothing too, and the log carries the distinction:
`decision=model_answered` / `model_silent` / `fail_closed_action_error` /
`fail_closed_llm_error`.

## Socket

The server binds `host:port` with SO_REUSEADDR, because hosts share 3702 between discovery
daemons. It then joins 239.255.255.250 when `join_multicast` (default true), best effort.

The default host is loopback, as for every server, so a responder meant to be found on a LAN
is started with host `0.0.0.0`; the server warns when it joins on loopback.

Its own announcements looping back are ignored by MessageID (the last 64). ProbeMatches and
ResolveMatches overheard on the group are ignored.

## Bounds

Every bound below drops the datagram with a WARN and serving continues; there is no error
reply to send.

- A datagram is at most 65 507 bytes (`MAX_DATAGRAM`).
- Elements nest at most 32 deep (`MAX_DEPTH`).
- Types, Scopes, XAddrs and match lists hold at most 64 entries each (`MAX_ITEMS`).

## Not implemented

- Discovery proxy (managed mode).
- IPv6 group `[FF02::C]`.
- The Probe/Resolve retransmission schedule (a match is sent once).
- Signed messages (the compact signature format).
