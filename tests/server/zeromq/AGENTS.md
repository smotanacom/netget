# ZeroMQ server tests

- `wire_test.rs`: a raw ZMTP peer — REP with a multipart request and its envelope, PING/PONG,
  binary frames as hex, a request without a delimiter closing the connection; ROUTER with two
  messages in flight from a DEALER with an Identity; PULL seeing a pushed message and sending
  nothing. Then the refusals (an incompatible socket type answered with ERROR, a CURVE
  greeting, a non-ZMTP greeting), the bounds (a frame announced over 1 MiB, 65 frames, an
  idle peer) and a REP request with no handler closing without a reply.
- `real_client_test.rs`: pyzmq (libzmq 4.3.5) as REQ, DEALER to ROUTER (with a REQ to the
  ROUTER too) and PUSH to PULL; go-zeromq/zmq4 (pure Go) as REQ and PUSH. zmq4 always sends a
  random UUID Identity, which the test checks by length.

Peers come from `tests/client/zeromq/install_peers.py`. No LLM calls.
