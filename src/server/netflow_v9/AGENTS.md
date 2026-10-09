# NetFlow v9 UDP collector — Experimental

`mod.rs` owns an RFC 3954 version 9 UDP parser/cache task and a separate common
handler dispatcher. Removing the server aborts both tasks, socket, bounded queue
and parked intercepts. The default port is2055, a common deployment convention
([Cisco collector documentation](https://docs.crossworkassurance.cisco.com/docs/netflow));
RFC3954 does not mandate a destination port. `collect_netflow_v9_records`
observes typed values in the standard access log; there is no protocol flow
store, aggregation, persistence or acknowledgment. The default `llm_fallback`
is false; explicit static/script/manual/model handlers still execute. Success,
empty/common-only actions, failed actions and backend errors have distinct
terminal `decision=` tags with `udp_silent=true`. Failed actions cannot roll
back common side effects or previously committed parser state.

`connectionless` stays false: the dispatcher creates one temporary dashboard
connection row while a datagram's common handler runs, then explicitly removes
it on success, silence or failure. The shared 10-second idle sweep must preserve
an aged row while a manual/model request is parked. No per-remote dashboard rows
await that sweep; template/session state has its separate owned expiry timer.
The feature-gated connectionless audit exception records this ownership rule.

This is a separate native wire format alongside IPFIX. The header is 20 bytes:
version9, total record Count, sysUpTime milliseconds, UNIX export seconds,
packet sequence and Source ID. Count includes normal/options template records
and data/options records, never FlowSets. Known-record Count mismatches fail.
Unknown-template sets expose ID/byte count without buffering their data; Count
is then `unverifiable_unknown_template`. Packet sequence still advances by one,
including template-only packets, modulo2^32. Gaps report missing packets;
duplicate/late packets do not regress the expectation or clock diagnostics.
Neither sequence nor Count implements recovery or duplicate suppression.

The collector socket is implicit in the cache. Source IP and Source ID isolate
template state as RFC3954 describes; UDP source ports deliberately do not.
Templates apply in FlowSet order and expire after `template_ttl_seconds`
(default600); identical definitions refresh and different definitions replace
immediately. Sessions expire after `session_idle_seconds` (default1800).
Both parameters accept1..86400 and expiry runs on an independent one-second
tick as well as ingest. A backwards UNIX clock on an advancing packet clears
candidate templates before parsing that packet. A lower sysUpTime is flagged
only: wrapping uptime, exporter reboot and delayed datagrams are ambiguous.
Restart sequences cannot be reliably distinguished from old packets. New
received definitions still replace old definitions in arrival order; there is
no special withdrawal encoding in this v9 implementation. Malformed packets
never partially commit template, sequence or clock changes, apart from the
independent expiration of already stale entries.

FlowSet0 carries normal templates; FlowSet1 options templates use scope/option
**descriptor byte lengths**, with the five RFC scope types in a separate
namespace. The codec has35 selected RFC fields covering IPv4/IPv6 addresses,
ports, byte/packet/flow counters, interface/AS identifiers, masks, flags,
relative first/last switched times, sampling, VLAN and direction. Exactly
one `element` or `scope` describes each exported field. Unknown field types
retain full16-bit IDs and fixed lengths and discard values into ordered null
slots; no enterprise-bit parsing, enterprise IDs, raw/base64 values, strings,
variable-length marker or complete vendor extension model is implemented.
Counters permit1..native maximum bytes; fixed scalars use their defined width.
Supported scopes are unsigned1..8 bytes. Minimum record size is4 bytes so
padding cannot masquerade as complete small records. Up to3 trailing padding
bytes are accepted, including nonzero values; longer incomplete records fail.
This restriction excludes valid shorter records and is part of the selected
scope, not a claim of full RFC compatibility.

Bounds:8192 bytes,64FlowSets,32template records,256data/options records,
32fields/template,1..1024bytes/fixed field,128source sessions,32templates/session,
1024templates globally. The handler queue holds32 events plus one active
handler; overflow is logged and discarded after parser state is committed.
Receive, invalid packet, queue-full and dispatcher-closed failures are silent
with actual terminal tags. There is no unsolicited server message handle.

Required independent peers are unmodified softflowd1.1.1 and actual GoFlow2
2.2.7. Softflowd is compiled with its upstream `ENABLE_LEGACY` option because
the default sender incorrectly counts data records only. No C source is
changed. Its actual offline-pcap v9 output is compared with a212-byte literal
(runtime UNIX header seconds checked independently then normalized), native
typed values and GoFlow2's independent decode. Sampling options are checked
separately against literal scope semantics. The peer fixture pcap is a small
synthetic input, not a captured production trace or NetGet capture facility.
Softflowd's BSD2/BSD3/ISC notices and GoFlow2's BSD3 license remain in owned
peer storage. Neither peer is vendored or linked into NetGet; no Cargo
protocol dependency is added. Missing peers fail tests rather than skip.

Primary references: [RFC3954](https://www.rfc-editor.org/rfc/rfc3954.html),
[softflowd1.1.1 source](https://github.com/irino/softflowd/tree/softflowd-v1.1.1),
[GoFlow2 2.2.7](https://github.com/netsampler/goflow2/releases/tag/v2.2.7).
No TCP/SCTP/TLS/DTLS, authentication, full vendor model, delayed-data recovery,
flow capture/storage, durability, fuzz or production pcap evidence is claimed.
The scope remains Experimental and is not a full RFC3954 implementation.
