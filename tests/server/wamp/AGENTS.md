# WAMP tests

Peers: `python3 tests/server/wamp/install_peers.py ROOT` builds `nexus/` (nexus 3.3.0, pinned by
go.sum) and installs autobahn-python 24.4.2 (hash-pinned pure wheel); it prints
`NETGET_WAMP_NEXUS` and `NETGET_WAMP_PYTHON`. `peer.py` drives autobahn; `tests/helpers/wamp.rs`
holds the router policy, raw WebSocket helpers and the nexus router launcher.

- `peer_test.rs` — autobahn: register and call through the router, its callee's own error
  routed back, acknowledged publish received by itself, a prefix subscription, the router's
  `com.example.time` and `com.example.forbidden`, a duplicate registration, realm "blocked",
  GOODBYE. nexus: join, register and call, publish and receive, the router procedures, "blocked".
- `wire_test.rs` — a msgpack-only handshake refused, HELLO rules and timeout, wildcard and
  exact subscriptions with exclusion and black/white listing, routed RPC with disclose_me,
  errors routed back, unknown subscription and registration, a callee leaving mid-call, an
  injected publication, HELLO after WELCOME; ABORT and ERROR when the handler gives no answer;
  the NetGet pair (caller, callee, publisher, subscriber, GOODBYE).

`tests/client/wamp/peer_test.rs` — NetGet's client against the nexus router.
