# Consul server tests

`store_script` (in `wire_test.rs`) keeps KV entries (with modify indexes) and services in a
JSON file; keys under `locked/` are refused with 403.

- `wire_test.rs`: put with flags, get (base64 value, raw, keys, recurse, binary values via
  hex), check-and-set stale and current, a 403, delete and 404, registration through catalog,
  health and agent services, deregistration, and Rust's own status/self answers; then an
  oversized body (413), a bad registration, unknown endpoints, a wrong method, and an
  unreachable model (500, never a value or `true`).
- `real_client_test.rs`: the consul 1.20.2 CLI (kv put/get/-detailed/-keys/-recurse/-cas,
  delete, services register/deregister, catalog services) and py-consul 1.7.1
  (`py_consul_peer.py`), which found the case-sensitive registration decoding.

Peers from `install_peers.py`. No LLM calls.
