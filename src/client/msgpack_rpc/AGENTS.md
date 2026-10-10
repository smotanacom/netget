# MessagePack-RPC client

Uses the server's `wire.rs`. One connection; a reader task decodes whole messages so a
partial one is never lost to `select!`.

- `msgpack_call {method, params}`: calls may be pipelined (at most 64 awaiting); each
  `msgpack_response {method, msgid, result, error}` is matched by msgid, and a response to no
  call is dropped.
- `msgpack_notify {method, params}`; the server's notifications arrive as
  `msgpack_notification`.
- A request from the server is answered with an error: this client serves no methods.

Same JSON mapping and bounds as the server.
