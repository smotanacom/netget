# HTTP/3 client

Feature `http3`, native authenticated reusable QUIC connection with ALPN `h3`.
quinn 0.11/rustls 0.23/h3 0.0.8/h3-quinn 0.0.10. Connect performs a real TLS
handshake and returns the actual bound local UDP endpoint. System roots are used
plus optional PEM ca_cert_path; server_name overrides the authentication name.
No certificate verification bypass exists. Generated localhost certificates must
be explicitly trusted. remote_addr is host:port or bracketed IPv6:port.

Actions: send_http3_request(method, origin-form path, headers, UTF-8 body,
trailers, priority 0..7), wait_for_more, disconnect. Startup default_headers are
merged case-insensitively under per-request headers. Priority sends RFC 9218
`priority: u=N`; zero is most urgent. Trailers are followed by an explicit FIN.
Foreign absolute URLs are refused; requests always use the connected target.

Events: http3_connected (real authenticated session), http3_response_received
(status_code, headers, UTF-8 body, trailers, stream index), http3_request_failed
(local failure). Repeated header values are preserved as arrays. Every event
advertises its executable semantic actions and follows shared client dispatch.
The original instruction and every response action reach the wire; follow-ups
are capped at four levels while final responses still reach handlers.

The single registered owner polls the control/QPACK driver, up to 32 total request
and event-handler futures, plus the bounded injected-command channel. Every
accepted request reserves its response-handler slot, so parked manual decisions
cannot discard later responses; further queries return a busy error at capacity.
A parked manual response or stalled request cannot prevent disconnect or another
request. Injected unknown actions return Rejected; completed exchanges return
Executed with actual status/body size, never an invented datagram byte count.
Disconnect and removal drop every request/handler and close the endpoint.

Bounds: 32 KiB header/trailer field sections including pseudo-fields, 8 MiB UTF-8 body, handshake timeout
10 seconds (1..60), exchange timeout 30 (1..300), QUIC idle 300 (1..3600).
Timeout cancels only the request; the connection can carry later requests. Body
and header limits apply in both directions. Counters count semantic body bytes.

Experimental: live independent aioquic 1.3.0 server tests, NetGet pair tests,
certificate/name negatives, concurrent requests, bounds and cancellation/removal.
See tests/client/http3/CLAUDE.md. No 0-RTT, migration, server push, DATAGRAM,
WebTransport, binary-body action, second independent implementation, pcap oracle
or fuzz target. The local h3-quinn patch prevents cancellation of a Pending read
from panicking; upstream source/license and a dedicated fail-before/pass-after
regression are retained.
