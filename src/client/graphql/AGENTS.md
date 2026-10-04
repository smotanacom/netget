# GraphQL client — Experimental, GraphQL over HTTP and graphql-transport-ws

Targets `http://<remote_addr><endpoint>` through the shared `http_fetch` client (no redirects).
With `introspect` (default on) it runs one introspection query on connect and raises
`graphql_connected` with root-field signatures per operation type (`book(id: ID!): Book`), or
`introspection_error` saying why there is no schema — advisory, never fatal; a transport failure
is. `graphql_query` is syntax-checked by apollo-compiler (operation picked by name, subscriptions
refused), sent as POST JSON or, with `use_get`, as a GET with URL parameters (mutations refused
over GET), with `Accept: application/graphql-response+json, application/json;q=0.9`. The answer
must be one of those media types and a well-formed GraphQL response (`data` and/or non-empty
`errors`; no data with 2xx under graphql-response+json); otherwise `graphql_response` carries
`error` instead. 1 MiB answers, 30 s per request. Shares `src/server/graphql/engine.rs`.

`graphql_subscribe` (subscription operations only) opens `ws://<remote_addr><endpoint>` with the
`graphql-transport-ws` subprotocol on first use — the server must agree to it — and completes
`connection_init`/`connection_ack` within 10 s, answering pings. Ids are Rust-assigned
(`1`, `2`, …); 64 active at most. `next` raises `graphql_subscription_event` (payload checked as a
GraphQL response), `error` raises `graphql_subscription_error`, `complete` raises
`graphql_subscription_complete`; messages for ended ids are ignored. `graphql_unsubscribe` sends
`complete`. If the socket closes, every active subscription gets a `graphql_subscription_error`
naming why. Events are delivered with backpressure (the socket is not read while the handler is
behind).
