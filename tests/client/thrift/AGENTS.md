# Thrift client tests

`peer_test.rs` runs `tests/server/thrift/peer.py serve`: a thriftpy2 0.7.1 server (independent,
unchanged) for `users.thrift`, framed binary and then buffered compact. NetGet's client calls
every function — a sum, a struct, a declared exception, a list from a set of enums, a struct
argument and a oneway call the server prints, and a function the server does not have (refused
with UNKNOWN_METHOD) — and is refused, before the wire, an argument missing a required field.
Needs `NETGET_THRIFT_PYTHON` from `tests/server/thrift/install_peers.py`.
