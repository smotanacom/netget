# IPFIX UDP exporter — Experimental

`export_ipfix_records` accepts a typed `batch`: unsigned observation domain,
optional unsigned export-time seconds, 1..32 templates and ordered data sets.
Templates have ID >=256, optional positive options `scope_count`, and ordered
supported element/optional length fields. Records are aligned arrays of
`{kind,value}` fields: unsigned, ipv4, ipv6, string, timestamp_seconds or
timestamp_milliseconds. See `actions::example_batch` and the collector's
`codec.rs` for the supported IANA subset. Extra schema/value properties fail.

Validation covers the complete batch before one UDP datagram is sent. Every
referenced template must be included in the batch; each batch repeats those
definitions ahead of records. The limits match the collector codec: 8192
bytes, 64 sets, 256 data/options records, 32 fields and 1024-byte strings.
Reduced unsigned values must fit their chosen length. Fixed strings must have
exact UTF-8 byte length; addresses/dates have their normative widths. Unknown
enterprise fields and raw/base64 values are not exportable.

`transport::Catalog` holds only active template definitions and sequences, at
most 32 domains and 32 templates per domain. Template-ID redefinition within
the socket is rejected; reconnect with a new UDP source port to establish a new
transport session. Candidate catalog state commits only after local send
success. Sequence increments by data/options records modulo 2^32, independently
per domain; template-only retransmissions do not increment it. Every active
template is periodically retransmitted (`template_refresh_seconds`, 60 default,
1..3600). Definitions remain active until disconnect; there is no flow store.

One owned task polls commands, sends, refresh ticks and handlers independently.
Manual parked handlers do not stop injection or refresh. Resolution and an
entire write operation have ten-second deadlines; one send is in flight.
Client event/action queues each hold at most 32 and followup depth is at most 8.
Capacity exhaustion closes the client; invalid individual actions are rejected.
Unsolicited collector datagrams and UDP transport errors close the one-way
session. `disconnect` and removal cancel all owned work and parked intercepts.

`ipfix_connected` means the UDP exporter is ready. `ipfix_exported` contains
domain, header sequence, template/record/byte counts and
`local_transport_only: true`. An executed command confirms local UDP transport
acceptance. It does not confirm collector receipt, decoding, persistence or
processing. Refreshes enter the common access log without creating handler
followups. Standard static/script/manual/model handlers and client memory use
the common dispatcher; blank instruction without a handler never calls a model.

Required peer tests use the public Python ipfix 0.9.7 decoder and actual
GoFlow2 2.2.7 service, compare all exported value classes and options sequences
against independent expected values, and fail if a required peer is missing.
The collector docs link the normative sources and licenses. No protocol Cargo
dependency is added. There is no SCTP/TCP/TLS/DTLS, collector authentication,
ACK, data retry, template-ID reuse, full IANA model, storage, fuzz or pcap
evidence; this scoped implementation remains Experimental.
