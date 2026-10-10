# Pulsar broker tests

`install_peers.py <dir>` installs Apache Pulsar 4.0.6 (SHA-512 checked) and the official Python
client 3.13.0 in a venv, and prints `NETGET_PULSAR_HOME` and `NETGET_PULSAR_PYTHON`. The tests
fail without them. Java 17+ is needed for the Java CLI.

- `the_python_client_produces_and_consumes_through_netget` (`py_session.py`, the C++ library):
  two subscriptions, a named producer with properties and a key, a non-ASCII payload, a message
  the model refuses (the client fails that send, logs the model's reason, and does not
  reconnect — one producer event per producer), then a batching producer whose three messages
  arrive as three. Every accepted message is read back in order, and the model's own
  `pulsar_publish` answers arrive on `replies`.
- `the_java_cli_produces_and_consumes_through_netget`: `bin/pulsar-client consume` and
  `produce` (with a property), both through NetGet; a refused message fails with
  `NotAllowedException` and the model's text.
- `checksums_bounds_and_a_failed_handler`: raw frames — producer and request id 0 (required
  zero fields on the wire), a corrupted checksum (`ChecksumError`, no event), the intact frame
  (receipt for entry 0), a frame announcing more than `MAX_FRAME`, and no model (producer
  refused with a category).

No LLM calls.
