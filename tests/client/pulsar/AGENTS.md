# Pulsar client tests

`real_server_test.rs` starts Apache Pulsar 4.0.6 standalone (broker, BookKeeper and a RocksDB
metadata store in one JVM; loopback ports of its own; `-nss -nfw`) from
`tests/server/pulsar/install_peers.py`, and waits for `public/default` over the admin API.

A python chain subscribes NetGet to `inbox` on connect and answers each message with a
message on `outbox` carrying a key and a property derived from it. The official Python client
publishes two messages to `inbox` and reads NetGet's two answers from `outbox`. Then an injected
hex payload is stored (receipt asserted) and an invalid topic is refused locally.

Mutation-checked: dropping the model's actions fails the test. No LLM calls.
