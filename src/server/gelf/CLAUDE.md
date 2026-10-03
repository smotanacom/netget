# GELF collector

Native GELF 1.1 collector (`gelf`, alias `graylog`), transport `udp` by default or `tcp`,
port 12201. Specification: https://archivedocs.graylog.org/en/latest/pages/gelf.html.
UDP accepts UTF-8 JSON, gzip and zlib, and the 12-byte GELF chunk header. Chunks correlate
by sender IP/port plus eight-byte ID; order is arbitrary, identical duplicates cost no
additional memory, conflicting duplicate/count changes discard the message. Completion,
conflict and expiration retain IDs for another five seconds in a bounded 256-entry map.
Reusing an ID within this interval suppresses the message. TCP accepts only uncompressed
JSON terminated by NUL, with fragmented/coalesced frames supported.

`gelf_message` carries `message` (typed fields, additional_fields without leading underscores),
`source_addr` and `transport`. Version must be 1.1; host/short_message nonempty; timestamp
finite and nonnegative; level 0..7. Missing timestamp resolves to receiver time and missing
level to ALERT (1). Deprecated facility/file/line are supported. Additional fields accept
strings/numbers with alphanumeric, underscore, dot or dash names; `id` is rejected. Unknown
unprefixed fields and non-string/number additional values are rejected. No raw/base64 event
or action data. No protocol database or private persistent storage.

Unmatched messages collect directly in the standard bounded access log without model calls,
even with a nonempty instruction. Explicit static/script/manual/model rules always dispatch;
`llm_fallback=true` opts unmatched messages into the model. `collect_gelf_message` observes;
common actions update standard server memory through the shared dispatcher. There is no
reply or delivery acknowledgment. TCP handler failure closes the peer, UDP logs and continues. A mixed action batch with any
failed action records a decision-tagged gelf_handler_failed event and fails closed even if
a collect action succeeded; shared common-action side effects are not rolled back.

Metadata declares `request_only`: GELF is a one-way log stream; collecting a received message has no reply or unsolicited server-message operation. The dashboard and MCP peer-message actions show this reason instead of offering an unsolicited send.

Bounds: 256KiB encoded/decompressed/compressed JSON; 8192-byte UDP datagram; 128 chunks
within five seconds; 128 in-progress messages and 4MiB stored chunk payload, plus bounded
metadata/recent IDs. A periodic sweep removes incomplete UDP state even without new packets.
TCP has 256 connections and a 30s absolute deadline to finish each frame (trickled bytes do
not reset it; handler time excluded), with <=256KiB+8192 buffered bytes. All listener,
collector and per-TCP-peer tasks are registered. Stop drops socket and all tasks, including
manual waits; UDP peer rows exist only during complete-message handling. Metadata deliberately
omits connectionless because the role also owns TCP sessions and chunk state.

Experimental. Both transports have independent emitter/receiver evidence, but no Stable
rating, fuzz execution, pcap oracle or complete Graylog service claim. No HTTP input, TLS,
authentication, persistence, query API, acknowledgments or retry.
