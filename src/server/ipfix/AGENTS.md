# IPFIX UDP collector — Experimental

`mod.rs` owns an RFC 7011 version 10 UDP listener (default 4739). One owned
task parses datagrams and maintains the transient template cache; a second
owned task dispatches typed `ipfix_message` events through the common handlers,
memory and access log. Removing the server cancels both tasks and their socket,
queue and parked intercepts. `collect_ipfix_records` adds no protocol storage.

Default `llm_fallback` is false. A matching static/script/manual/model handler
or explicit fallback enables common dispatch. Every message is deliberately
silent: there is no UDP response or acknowledgment. Terminal status logs include
`decision=default_collect`, `handler_collect` for successful explicit collection,
or `handler_silent` for empty/common-only action batches. Failed actions produce
`fail_closed_action_error`; backend/handler dispatch errors produce
`fail_closed_dispatch_error`. The existing `ipfix_handler_failed` access-log
decision remains `fail_closed_handler_error`, with `terminal_decision` carrying
the precise cause. Common actions already applied are not rolled back. Without
a handler, valid events enter the standard access log. Parsing
and cache expiry continue while a handler is parked. The dispatch queue holds
32 messages plus one active handler; excess messages are logged and discarded
after their wire-level template/sequence state has been processed. Invalid
datagrams, queue overflow, a closed dispatcher and receive errors also emit
terminal `fail_closed_` decision logs. Each terminal status log explicitly
records `udp_silent=true`; none acknowledges delivery, storage or durability.

`codec.rs` validates a whole datagram before committing candidate session state.
The collector socket is implicit in each cache instance; exporter IP, UDP port
and observation domain form its session key. New templates become available in
set order. Identical definitions refresh their lifetime; differing definitions
replace them. UDP template withdrawals are ignored as RFC 7011 requires.
Templates expire after `template_ttl_seconds` (600 default), sessions after
`session_idle_seconds` (1800 default); both accept 1..86400. An independent
one-second tick and each ingest expire state. Unknown-template data sets expose
only template ID and byte count and reset sequence tracking; records are not
buffered for later recovery.

Sequence numbers count data and options records modulo 2^32. Forward gaps report
missing records; duplicate/late messages are reported without regressing the
expected sequence. Template-only messages do not increment it. This is a
diagnostic, not reliable UDP delivery or duplicate suppression.

Bounds: 8192 bytes per datagram, 64 sets, 32 template records per datagram,
32 fields per template, 256 data/options records per datagram, 1024 bytes per
field, 128 sessions, 32 cached templates per session, 1024 globally. Known
unsigned fields permit reduced sizes 1..native width; addresses and date fields
require their defined width. Strings are UTF-8, fixed length or variable length
with RFC one/three-byte prefixes; fixed strings preserve NULs. Padding shorter
than the minimum record is accepted, including nonzero padding. Unsupported
standard/enterprise fields retain descriptors and discard values, exposing
`null` in their ordered record slot, never raw/base64 model data.

The selected IANA elements are the 35 explicit entries in `elements!`, covering
IPv4/IPv6 addresses, ports, protocol/TCP flags, counters, interfaces, AS numbers,
selected sampling/export/domain identifiers, UTF-8 names, seconds and
milliseconds. TCP control bits ignore the reserved upper four bits on input
and reject them on output. No list/subtemplate, boolean, float, signed, NTP,
opaque enterprise values or complete IANA information model is implemented.

Evidence is in `tests/server/ipfix` and `tests/client/ipfix`: literal wire golden,
native typed assertions, unmodified Python ipfix 0.9.7 exporter/decoder and an
actual official GoFlow2 2.2.7 collector. Python's old default IANA model is only
an external test peer. Its LGPL-3.0-or-later code is neither vendored nor linked;
GoFlow2 is BSD-3-Clause. NetGet adds no Cargo dependency for this protocol.

Primary references: [RFC 7011](https://www.rfc-editor.org/rfc/rfc7011.html),
[IANA IPFIX elements](https://www.iana.org/assignments/ipfix/),
[Python public API](https://github.com/britram/python-ipfix),
[GoFlow2 2.2.7](https://github.com/netsampler/goflow2/releases/tag/v2.2.7).
This bounded UDP subset has no SCTP/TCP/TLS/DTLS, authentication, full 65535-byte
RFC message support, data storage, ACK, durability, fuzz or pcap claim. It stays
Experimental and is not a full RFC-conforming IPFIX implementation.
