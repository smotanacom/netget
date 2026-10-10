# Cap'n Proto RPC client

Uses the server's `layout`, `schema` and `rpc` modules. `connect` loads the schema (same
`schema`/`interface` parameters), bootstraps within 10 s, and keeps the bootstrap capability
by finishing that question with `releaseResultCaps = false` — the default (true) releases it,
which NetGet's own server ignored and both real servers honoured (`unknown export ID`).

`capnp_call {method, params}` encodes the params by the schema (a method the interface lacks,
an unknown field or an out-of-range number is refused locally) and sends a Call on the bootstrap
import; calls are pipelined, at most 64 awaiting answers, matched by question id. Each Return
becomes `capnp_result {method, results, exception}` and is finished. `capnp_connected` lists
every method with its params and results fields. The client exports nothing: a Call or
Bootstrap from the server is answered `unimplemented`. A handler chain stops after 8 follow-ups.
