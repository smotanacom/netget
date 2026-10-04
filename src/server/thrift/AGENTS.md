# Thrift server — Experimental

The service is declared in IDL (`idl` startup parameter, up to 256 KiB; `service` picks one when
the IDL has several). `idl.rs` parses namespace, typedef, const (skipped), enum, struct, union,
exception and service (extends, oneway, throws); `include` is refused, as are duplicate field ids
and unknown types. `codec.rs` reads and writes the binary protocol (strict and old) and the
compact protocol, every type including uuid, with depth 64 and 16 MiB per message; a short buffer
is reported as `Incomplete`, never as malformed. `value.rs` converts between Thrift values and
JSON by the declared types: structs and exceptions as objects by field name, enums by label, maps
with string keys as objects (otherwise `[key, value]` pairs), binary as text when UTF-8.

Rust owns:
- The transport, detected on a connection's first byte: 0x80 or 0x82 is unframed (strict binary or
  compact); anything else is a 4-byte frame length. An old-style (non-strict) binary client must
  therefore be framed. An unframed message's length is only known by decoding it, so a partial
  one is decoded again once the buffer grows by half or the peer pauses 20 ms.
- The protocol, detected per message; the reply uses the request's.
- Refusals without asking the handler, as a TApplicationException: an unknown method
  (UNKNOWN_METHOD), a message that is not a call (PROTOCOL_ERROR), a
  missing `required` argument (PROTOCOL_ERROR). An undecodable message or oversized frame closes
  the connection.
- The result struct: field 0 for a return value, the throws field's id for a declared exception.

The handler answers `thrift_call` {service, method, oneway, args, returns, throws} with
`thrift_return` (value checked against the return type), `thrift_throw` (a throws field name and
the exception's fields), `thrift_error` (INTERNAL_ERROR with the message) or `thrift_ignore` (for
oneway). A value that does not fit the IDL, or no answer, is INTERNAL_ERROR; oneway calls are never
answered. Connections idle 120 s are closed.

Not implemented: the multiplexed protocol, JSON protocol, HTTP and THeader transports, TLS,
include, constants.
