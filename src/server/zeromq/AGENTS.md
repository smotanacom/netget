# ZeroMQ server (ZMTP 3.1)

Hand-written over Tokio TCP (`wire.rs`, shared with the client). The pure-Rust `zeromq`
crate was the alternative; it hides the handshake and the envelopes NetGet needs to show the
handler, and owning the codec keeps the bounds ours.

## What Rust owns

The 64-byte greeting (3.1 sent, 3.0 and later accepted), the NULL mechanism only (any other
is answered with an ERROR command naming it), READY with Socket-Type, the peer's Identity,
the compatibility table (an incompatible peer gets ERROR "REP cannot talk to PUB" and is
closed), multipart framing with short and long sizes, PING answered with PONG, and the
request envelope: a REQ peer's empty delimiter frame is stripped before the handler sees the
message and put back on the reply, for REP and ROUTER alike.

## Socket types (`socket_type`)

- `rep` (default): strictly one reply per request. A request without its delimiter is a
  protocol error; a request the handler does not answer closes the connection, because a
  REQ peer would otherwise wait forever and a fabricated reply would be worse.
- `router`: any number of messages in flight per connection, each answered or ignored; the
  peer's Identity is in the event.
- `pull`: receive only; `zmq_ignore` is the normal answer and a reply is dropped with a
  warning.

## Event and bounds

`zmq_message {socket_type, peer_socket_type, peer_identity, frames, encoding,
remote_addr}` → `zmq_reply {frames, encoding?}` or `zmq_ignore`. Frames are UTF-8 strings,
or all hex when any frame is binary. A frame or message over 1 MiB is refused from its size
field; 64 frames per message; the handshake has 10 s; each connection may be silent
`idle_timeout_secs` (default 300). Every outcome logs its `decision=` tag. ZeroMQ registers
no port, so no default is declared.
