# Raw QUIC server

Feature `quic`, protocol `QUIC`, default UDP/443. This is raw RFC 9000/9001
bidirectional stream I/O. ALPN is `netget-quic`; it never advertises `h3` and does
not parse HTTP/3 HEADERS/DATA or QPACK. The HTTP3 protocol is separate.

Events are `quic_connection_opened` (notification without stream actions),
`quic_stream_opened` and `quic_data_received`. Stream actions are `send_quic_data`,
`wait_for_more`, `close_this_stream`. No async server-send action is advertised:
responses belong to the stream which generated the event. Client injection is
available through the separate raw QUIC client.

Payloads have explicit encoding: utf8 by default, hex or base64. Received printable
ASCII is represented as utf8; other bytes use hex. Echo scripts must preserve both
`data` and `encoding`. Encoded action text is capped at 4 MiB before cleaning or
decoding; each decoded response action is capped at 1 MiB. Decode errors do not
include the full submitted payload. `wait_for_more`
does not replay its input; a handler can retain partial application messages in
memory. Stream data events follow transport reads, not application records.

The registered accept task owns all connection futures, which own all stream
futures. Endpoint and connection guards close transport state on abort. Limits
are 64 connections including pending handshakes, 32 streams per connection,
10-second handshakes, 30-second whole stream lifetimes and 300-second connection
idle timeout. Receive credit is bounded; unidirectional streams and migration are
disabled. Client STOP_SENDING cancels pending stream work. FIN or close_this_stream
finishes the response and releases the stream. Removing the server releases its
UDP endpoint and all owned sessions. Counters update against the actual connection.

TLS 1.3 is mandatory. Existing TLS parameters are supported: cert_path/key_path,
common_name, san_dns_names, validity_days, organization, organizational_unit.
There is no tls_enabled switch. Default generated localhost certificates must be
explicitly trusted by clients, or supply your own certificate/key pair.

Backend errors reset streams with application-local codes: unavailable 0x0102,
overload 0x0107. These numeric values preserve existing raw peer compatibility;
they do not imply HTTP/3 semantics. Internal error strings are only logged.
Cancellation uses application code 3. No request_filter, server-initiated streams,
unidirectional streams, DATAGRAM, 0-RTT, or HTTP/3 framing is implemented.

The server retains its existing Beta rating. Existing Quinn tests cover text and
binary echo, custom responses, concurrent streams and backend failure. An
additional independent aioquic 1.3.0 client checks certificate authentication,
binary payloads and concurrent streams. See `tests/server/quic/CLAUDE.md`.
