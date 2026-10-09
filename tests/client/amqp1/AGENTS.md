# AMQP 1.0 client tests

`peer_test.rs` runs `tests/server/amqp1/js/broker.cjs`: rhea 3.0.5 (independent, unchanged)
listening with SASL PLAIN for bob/pw. NetGet's client sends a message the broker accepts and
prints (body, subject, message-id, application property), one it rejects, one to an address it
refuses, receives the message the broker sends to queue.out, disconnects, and is refused a wrong
password. Needs `NETGET_AMQP1_NODE_MODULES` from `tests/server/amqp1/install_peers.py`.
