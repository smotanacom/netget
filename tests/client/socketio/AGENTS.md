# Socket.IO client tests

`peer_test.rs` runs python-socketio 5.17.0's AsyncServer (independent, unchanged, under
uvicorn) and drives NetGet's client over WebSocket and over polling: the greeting, an emit with
ack and the broadcast, an ack'd emit in `/chat` (auth required), a server event NetGet
acknowledges, a server-side disconnect, and a refused namespace. Needs `NETGET_SOCKETIO_PYTHON`
and `NETGET_SOCKETIO_NODE_MODULES` from `tests/server/socketio/install_peers.py`; fails without
them.
