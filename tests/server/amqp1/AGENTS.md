# AMQP 1.0 tests

Peers: `python3 tests/server/amqp1/install_peers.py ROOT` installs rhea 3.0.5 (`npm ci` from
`js/package-lock.json`) and builds `goamqp/` (go-amqp 1.7.0, pinned by go.sum); it prints
`NETGET_AMQP1_NODE_MODULES` and `NETGET_AMQP1_GOAMQP`. `js/client.cjs` drives rhea as a client,
`js/broker.cjs` runs rhea as a broker; `tests/helpers/amqp1.rs` holds the container policy.

- `peer_test.rs` — rhea: SASL PLAIN, receivers on orders.confirmed, news and chat, an accepted
  and a rejected order, the confirmation the handler routes, the message the handler produces for
  news, its own data message relayed on chat, a refused address, a refused password. go-amqp: a
  data-body and a value-body order accepted, one rejected, the confirmation, a refused address and
  password.
- `wire_test.rs` — type-system round trips, compact encodings and hostile input; the NetGet pair
  (outcomes, relay to a consumer, a produced message); no handler answer.

`tests/client/amqp1/peer_test.rs` — NetGet's client against the rhea broker.
