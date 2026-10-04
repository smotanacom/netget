# GraphQL client — Experimental, GraphQL over HTTP

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
