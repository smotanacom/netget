# Thrift tests

Peers: `python3 tests/server/thrift/install_peers.py ROOT` installs thriftpy2 0.7.1 (with ply
3.11 and ijson 3.4.0) and Apache Thrift 0.25.0 into a venv from hash-pinned wheels (CPython 3.10
on macOS arm64, CPython 3.12 on Linux x86_64) and prints `NETGET_THRIFT_PYTHON`. `peer.py` drives
them; `users.thrift` is the service both sides load; `tests/helpers/thrift.rs` holds the handler
policy.

- `peer_test.rs` — thriftpy2 (IDL-driven) over framed binary and buffered compact: a return, a
  struct, a declared exception, a list of structs from a set of enums, a struct argument, a oneway
  call, an application error, and a call after it. Apache Thrift's own protocol classes, no
  generated code, over buffered binary and framed compact: results read generically by field id,
  the exception in its throws field, a oneway call, and an unknown method refused with
  UNKNOWN_METHOD without reaching the handler.
- `wire_test.rs` — every value type through both protocols with every truncation incomplete, a
  10 000-level nesting bomb in each, IDL refusals, and from a socket: an unknown method, a frame
  over 16 MiB, a call sent one byte at a time, and two pipelined calls.

`tests/client/thrift/peer_test.rs` — NetGet's client against a thriftpy2 server.
