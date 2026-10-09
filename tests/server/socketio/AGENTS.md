# Socket.IO tests

Peers: `python3 tests/server/socketio/install_peers.py ROOT` installs python-socketio 5.17.0 and
python-engineio 4.14.0 (hash-pinned wheels; with requests, websocket-client and uvicorn) and
the reference socket.io-client 4.8.4 through `npm ci` from `js/package-lock.json`; it prints
`NETGET_SOCKETIO_PYTHON` and `NETGET_SOCKETIO_NODE_MODULES`. `peer.py` runs python-socketio
as client or server; `js/peer.cjs` runs socket.io-client. `tests/helpers/socketio.rs` holds the
chat policy.

- `peer_test.rs` — socket.io-client and python-socketio, each over polling upgraded to
  WebSocket and over WebSocket only: greeting, emit with ack, the broadcast, an ack the server
  asks for, `/admin` with and without the right auth, an unknown namespace, a server-side
  disconnect; the server saw a CONNECT over polling and one over WebSocket.
- `wire_test.rs` — codecs; EIO/transport/sid errors, the open packet, a polled CONNECT and
  greeting, an ack'd event, a refused binary event, rooms, a peer push, the two-poll protocol
  error, a ping timeout; a handler-less server answering CONNECT_ERROR; the NetGet pair over
  WebSocket and polling, including the client acknowledging a server event.

`tests/client/socketio/peer_test.rs` — NetGet's client against python-socketio's server.
