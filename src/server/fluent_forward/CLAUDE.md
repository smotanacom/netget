# Fluent Forward collector

Canonical FluentForward, feature `fluent-forward`, aliases fluent-forward/fluent_forward/
fluentforward/fluentd, TCP port 24224. Native unauthenticated Forward v1 transport supports
Message, Forward, PackedForward (bin and legacy str), gzip CompressedPackedForward (bin,
including concatenated gzip members), integer seconds and EventTime type-0 ext8 nanoseconds.
Reference: https://github.com/fluent/fluentd/wiki/Forward-Protocol-Specification-v1.
No security handshake or TLS; authenticated HELO/PING/PONG peers are unsupported.

`forward_batch` carries tag, entries [{timestamp:{seconds,nanoseconds?}, record:{...}}],
mode, record_count, ack_requested and source_addr. Records use JSON-shaped UTF-8 values;
binary and arbitrary extensions inside records are rejected. Opaque chunk tokens are never
model-facing raw/base64 data. The transport retains and echoes the token only when an ACK
was requested. `accept_forward_batch` acknowledges acceptance; `reject_forward_batch`
closes without ACK. Static empty actions mean successful handling. Any failed action in a
mixed result fails closed before ACK. Common actions update standard server memory.
Handler dispatch errors and failed actions record explicit fail_closed decisions; the
metadata declares deliberate silence because Forward has no negative ACK representation.

Unmatched batches use the standard bounded access log with no model calls even if instruction
is nonempty. Configured static/script/manual/model handlers always run; llm_fallback=true
opts unmatched batches into the model. No protocol-specific persistence, retry or deduplication;
ACK means parsing/handling succeeded, not durable delivery.

Metadata declares `request_only`: Forward acceptance or rejection requires the current inbound batch; ACKs must echo its transport-owned chunk token, so unsolicited replies are not supported. The dashboard and MCP peer-message actions show this reason instead of offering an unsolicited send.

Bounds: 256KiB complete frame/decompressed packed stream, 256 records, nesting depth32,
16384 decoded MessagePack values/record validation budget, tag1024 bytes, printable chunk token256 bytes,
256 TCP connections, 30s absolute frame-completion deadline and 10s ACK-write deadline.
Buffered TCP bytes <=256KiB+8192. Trickle does not reset a deadline and handler time is excluded.
All listener/peer tasks are owned; stop closes streams and cancels parked handlers.
Malformed/truncated/deep/size-bomb input closes only its peer. Nil TCP heartbeats are ignored.

Experimental. Real independent fluent-logger emission and official Fluentd reception/ACKs
provide both-role evidence. No Stable/fuzz/pcap claim. No UDP heartbeat listener, JSON convenience
framing, secure-forward authentication, TLS, opaque binary records or storage/query API.
