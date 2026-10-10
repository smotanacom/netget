# Consul server

The Consul agent HTTP API over hyper (port 8500, one request per connection), shapes measured
against Consul 1.20.2. NetGet stores nothing: the handler (its memory, or a script's file) is
the KV store and the service registry.

## Endpoints

- `/v1/kv/<key>`: GET (plain, `?recurse`, `?keys`, `?raw`), PUT (`?flags`, `?cas`), DELETE
  (`?recurse`). Values are base64 on the wire and text (or hex, `value_encoding`) to the
  handler; a write answers `true`/`false` as Consul does, and a 404 has an empty body.
- `/v1/catalog/services|service/<name>|nodes|datacenters`, `/v1/health/service/<name>`,
  `/v1/agent/services`, `/v1/agent/service/register|deregister/<id>`, `/v1/agent/self`,
  `/v1/status/leader|peers`. Rust builds every catalog, health and agent entry (node,
  datacenter, weights, a passing serf check) from the few fields the handler gives.
- `X-Consul-Index` counts accepted writes; a handler's `modify_index` drives check-and-set.

Registration bodies are decoded **without regard to key case**, as Consul does (Go's
encoding/json): py-consul sends lower-case keys, and the server refused them until it was
pointed at it.

## What the handler decides

`consul_kv_read {key, recurse, keys_only}` → `consul_kv_entries` / `consul_not_found`;
`consul_kv_write {key, value, value_encoding, flags, cas}` and `consul_kv_delete` →
`consul_ok` / `consul_refuse`; `consul_catalog {endpoint, name}` → `consul_services` /
`consul_instances`; `consul_register {operation, service | id}` → `consul_ok`. Any of them may
answer `consul_error {status, message}`.

## Failure modes and bounds

A handler failure or silence is a 500 with `WireFailure`'s text (`answers_on_failure`), never
a value or a `true`. Bodies are capped at 512 KiB (Consul's own value limit, 413 past it),
listings at 10 000 entries, 32 KiB / 64 headers, 30 s deadlines. Not implemented: blocking
queries (a wait returns at once), sessions and locks, ACLs, Connect, the DNS interface, watches.
