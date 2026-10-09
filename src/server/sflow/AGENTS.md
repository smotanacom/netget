# sFlow v5 UDP collector — Experimental

`mod.rs` owns a UDP listener (default 6343), a parser/expiry task and a separate
typed-event dispatcher. Both tasks belong to the common `AppState` server and
are canceled on removal. Parsing and bounded wire state continue while a
common manual/script/model handler waits. The queue holds 32 events plus one
active dispatch. `collect_sflow_samples` adds no protocol storage; the common
bounded access log and shared memory remain the application state.

The UDP listener has no transport session, but each dispatched datagram owns a
temporary connection row until its common handler completes, including while a
manual/model answer is parked. The dispatcher then removes the row explicitly.
Metadata leaves `connectionless` false so the generic idle sweep cannot hide a
live request. The separate bounded `SequenceCache` expires on its own one-second
timer; this diagnostic state does not depend on the row or the generic reaper.

Default `llm_fallback` is false. Matching handlers always dispatch; unmatched
valid messages are observed without a backend call. All UDP outcomes are
deliberately silent, including collection, empty answers and failures. Actual
terminal logs distinguish `default_collect`, `handler_collect`, `handler_silent`,
failed actions, dispatch/backend errors, parser errors and queue/socket failures.
Each server decision includes `udp_silent=true`. Failed actions do not roll back
common actions already executed or prior successfully parsed wire state.

`codec.rs` implements version 5 XDR datagrams with IPv4/IPv6 agent addresses and
all four compact/expanded flow/counter sample formats. Selected flow records are
1 (sampled header), 2 (Ethernet), 3 (IPv4), 4 (IPv6), 1001 (extended switch);
counters are 1 (generic interface), 2 (Ethernet), 5 (VLAN). Counter integers keep
their wire widths, including 64-bit values above JSON's floating-point range.
Compact source IDs use class in the high 8 bits and index in the low 24;
interface format occupies the high 2 bits. Expanded forms preserve 32-bit
indices. Input unknown enterprise/format records expose only tags and byte count;
their values are discarded. Export actions accept only typed selected records.

Captured header bytes are never exposed to model data. The collector extracts
Ethernet/MAC, up to two VLAN tags, IPv4/IPv6 addresses, IP length/protocol/traffic
class and available TCP/UDP ports/TCP flags. It walks at most eight IPv6 extension
headers; non-initial fragments do not supply transport ports. Truncated,
unsupported and malformed packet headers retain safe metadata/status summaries;
the enclosing valid XDR record can still be observed. Packet/transport checksums,
payload contents and capture integrity are not verified.

Bounds: 8192 bytes/datagram, 32 samples, 64 records/sample, 256 total records,
256 captured header bytes, 4096 bytes/unknown opaque structure and 128 transient
sessions. Whole datagram decoding succeeds before sequence state changes.
The implicit collector plus exporter IP/UDP port, declared agent address and
sub-agent identify a session. `session_idle_seconds` is 1800 by default, accepts
1..86400, and expires on ingest and an independent one-second tick.

Datagram sequence numbers count datagrams modulo 2^32. Forward gaps report a
missing count; late/duplicate datagrams do not regress the next expectation.
A modular uptime decrease is diagnostic only: a reboot and a late datagram can
look alike. It never resets sequence expectation; idle expiry does. Sample
sequence numbers are passed through. They belong to sFlow instances, which need
not correspond one-to-one with a data source, per the official errata.

Independent evidence uses unmodified BSD-3-Clause Cistern/sflow source pinned to
`ed105e3cf9fb208505ed3a9939c9449321cbacf1` and actual official GoFlow2 2.2.7.
The Cistern source-ID encoder reverses the field shifts. The test command
compensates through public arguments only; actual emitted bytes must match a
literal oracle and GoFlow2's decoded source fields. Its VLAN encoder advertises
32 bytes but writes 28 and is excluded. The independent decoder correctly reads
the normative 28-byte VLAN counter; GoFlow2 reports it opaque, so that service
test asserts literal bytes. No native/self-roundtrip alone is interoperability
evidence. Peers stay in owned temporary storage; no protocol Cargo dependency.

Primary sources: [sFlow v5](https://sflow.org/sflow_version_5.txt),
[errata](https://sflow.org/developers/errata.php),
[structure registry](https://sflow.org/developers/structures.php),
[pinned Cistern API](https://github.com/Cistern/sflow/tree/ed105e3cf9fb208505ed3a9939c9449321cbacf1),
[GoFlow2 2.2.7](https://github.com/netsampler/goflow2/releases/tag/v2.2.7).
This manually supplied telemetry subset has no SNMP configuration, automatic
sampler/counter poller, packet capture, aggregation/store, authentication,
acknowledgment, reliability, v2/v4 compatibility, fuzz/pcap or full-compliance
claim. It remains Experimental.
