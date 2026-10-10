# MessagePack-RPC server

Hand-written over Tokio TCP (`wire.rs`, shared with the client): no MessagePack crate is in
the tree, and the ones that are common decode from a stream and allocate what a length field
announces. Here bytes are buffered (at most 1 MiB) and a value is decoded only from a complete
in-memory buffer, so no announced length allocates more than has arrived; every count is
checked against the bytes remaining, nesting stops at 32 (verified by raising the bound: the
test then gets a reply it must not).

## JSON mapping

nil/bool/int/float/str/array/map map to JSON directly; binary becomes `{"$bin": hex}`, an
extension `{"$ext": type, "hex": hex}` (Neovim's Buffer/Window/Tabpage handles are 0/1/2),
and a map with non-string keys uses each key's JSON text. The same objects encode back.

## What the handler decides

`msgpack_request {method, params, msgid}` → `msgpack_result {result}` or
`msgpack_error {error}`, optionally with `msgpack_notify {method, params}` actions sent after
the response; `msgpack_notification` → `msgpack_ignore` or `msgpack_notify`. Requests on one
connection are answered in order; a response from the client is a protocol error (the server
sends no requests).

## Failure modes and bounds

A handler failure, silence or wrong answer on a request is an error response carrying
`WireFailure`'s text, never a result (`answers_on_failure`), with its `decision=` tag. A
malformed value, a non-envelope, a message past 1 MiB or nesting past 32 closes the
connection; a message must complete within 30 s of its first byte; a connection may be
silent `idle_timeout_secs` (default 300). MessagePack-RPC registers no port.
