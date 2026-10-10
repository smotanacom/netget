# Consul client tests

The client's handlers put `app/config` (flags 5) on connect, then read it back and register
`netget-web` when the put is confirmed.

- `session_test.rs`: NetGet's own agent — the chain, an injected catalog query, a 404 and local
  refusals.
- `real_server_test.rs`: `consul agent -dev` 1.20.2 on probed loopback ports; the consul CLI
  (the agent's own view) confirms the key, its flags and the tagged service, then an injected
  health query, a delete and the 404 after it.

Peers from `tests/server/consul/install_peers.py`. No LLM calls.
