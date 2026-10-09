# ZeroMQ client (ZMTP 3.1)

Uses the server's `wire.rs`. One connection to `remote_addr` with the socket type from
`socket_type` (`req` default, `dealer`, `push`, `sub`) and an optional `identity`; the
handshake checks the server's type is compatible. A reader task feeds whole messages to the
session, so a partially read frame is never lost to `select!`.

- `zmq_send {frames, encoding?}`: REQ adds the envelope and refuses a second send until the
  reply arrives (the refusal is a `Rejected` outcome, nothing is sent); SUB cannot send.
- `zmq_subscribe` / `zmq_unsubscribe {topic}`: SUB only, sent as ZMTP 3.0 subscription
  messages (01/00 + prefix), which 3.1 publishers also accept.
- Received messages arrive as `zmq_message {frames, encoding}`: a REQ socket's reply without
  its envelope; anything a REQ socket did not ask for, and anything sent to a PUSH socket, is
  dropped.

No reconnection: when the server closes, the client ends. Same 1 MiB / 64-frame bounds.
