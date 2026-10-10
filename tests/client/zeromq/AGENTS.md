# ZeroMQ client tests

- `session_test.rs`: against NetGet's own server — REQ to REP with the second send refused
  before its reply and a subscribe refused on a REQ; DEALER with an Identity to ROUTER; PUSH
  of a hex frame to PULL, seen by the server's handler as hex.
- `real_server_test.rs`: pyzmq sockets through `peer.py` — REP, ROUTER (which reports the
  Identity it saw), PULL, and an XPUB that publishes only after it sees the subscription, so
  receiving "weather" frames and never "sports" proves the subscription crossed the wire; the
  unsubscribe is seen by the XPUB too.

`install_peers.py ROOT` installs pyzmq (hash-pinned) and builds `peer/` (go-zeromq/zmq4,
go.sum-pinned), printing NETGET_ZEROMQ_PYTHON and NETGET_ZEROMQ_GO_PEER for both suites.
