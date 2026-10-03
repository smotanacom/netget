# GELF emitter

Native GELF 1.1 client (`gelf`, `graylog`), port 12201. `transport=udp` default; `tcp`
uses NUL-delimited JSON. `compression=auto` means gzip on UDP and none on TCP. UDP also
supports none/zlib. Explicit gzip/zlib on TCP is rejected before opening a connection.
`chunk_size=1420` default, valid 13..8192 bytes including the 12-byte header; setting is
validated on both transports and applies to UDP. Each send has a fresh random eight-byte ID.

`gelf_connected` declares `send_gelf_message` and `disconnect`; send takes a structured
`message` object with host/short_message and optional full_message, timestamp, level,
facility/file/line, additional_fields. The codec validates the whole message and compression/
chunk plan before sending any bytes, with 256KiB message and 128-chunk limits. Transport
writes have one 10s total deadline; connect/resolve have 10s deadlines. TCP partial failures
close the connection; no automatic retries. Bytes_sent counts JSON, compression and framing
actually handed to the transport. Success confirms local acceptance, not remote persistence.

Command registration and the owned session task precede connected-handler dispatch, so
injection/disconnect work while a manual/model response is parked. Explicit static/script/
manual/model handlers use the standard client dispatcher and memory. With empty instruction
and no handler, no model call occurs. TCP also watches peer EOF/unexpected reply data;
UDP has no remote liveness/ack signal. The socket and command handle are released on exit,
stop or injected disconnect. No protocol-specific persistent storage.

Experimental. Independent official Graylog go-gelf source readers decode both transports;
source fixture tests are additional wire/lifecycle coverage. No TLS, HTTP, authenticated
Graylog service, persistence, queries, delivery acknowledgments or complete platform claim.
