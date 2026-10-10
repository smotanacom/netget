# Zipkin client tests

The reporter's connect handler reports a two-span gzip trace (`session_test::connect_report`),
so every test asserts the handler's answer on the server's side of the wire.

- `session_test.rs`: against NetGet's own collector — the report, injected queries (services,
  a trace, a 404), local refusals of a bad span, an empty report, an unknown endpoint and an
  undeclared parameter; and the follow-up bound, counted from the server's access log
  (verified: without it the chain ran 56 queries before the test stopped it).
- `real_server_test.rs`: the official zipkin-server 3.5.1 jar (bound to loopback through
  `armeria.ports[0]`), read back through its read API including spans, remoteServices, traces
  and a 404; and Jaeger 1.62 all-in-one's Zipkin collector (every other Jaeger listener on an
  OS-assigned loopback port), read back through Jaeger's own query API.

Peers from `tests/client/zipkin/install_peers.py` (Java 17+ on PATH). No LLM calls.
