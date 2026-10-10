# Cap'n Proto RPC server tests

`directory.capnp` is the shared schema (an enum, a union, a group, defaults, Data, nested
lists of structs, a superclass); `DIRECTORY_SCRIPT` (in `wire_test.rs`) answers it. The tests
need the `capnp` compiler on PATH and fail naming it when it is absent.

- `wire_test.rs`, with NetGet's own encoder as the client: a call pipelined on the bootstrap
  answer before it returns, a superclass method, the mapping round trip, an exception, an
  unknown method id, an unexported import, an unhandled message type echoed as
  `unimplemented`, a precompiled schema; then more than 64 segments, an oversized segment
  table, 1000 pointers aliasing one 64 KiB text (traversal), entries nested 100 deep (and 20,
  answered), results that do not fit the schema and an unreachable model (exceptions).
- `real_client_test.rs`: pycapnp 2.2.4 (`pycapnp_peer.py`) and capnproto.org/go/capnp
  v3.1.0-alpha.2 (`tests/client/capnp_rpc/peer`), each calling every method including five
  calls in flight.

Peers from `install_peers.py`. No LLM calls.
