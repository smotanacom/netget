# GraphQL tests

Peers: `python3 tests/server/graphql/install_peers.py ROOT` (gql 4.4.0 with its requests
transport, strawberry-graphql 0.330.2 with its asgi extra, uvicorn 0.34.0; hash-pinned wheels;
Python ≥ 3.10) prints `NETGET_GRAPHQL_PYTHON`. `peer.py client URL` runs gql unchanged;
`peer.py server` serves a strawberry bookstore and prints its port. `tests/helpers/graphql.rs`
holds the matching SDL and a script policy for NetGet's server.

- `peer_test.rs` — gql builds its schema from NetGet's introspection (graphql-core's
  `build_client_schema`), validates locally and runs variables, aliases, a union with
  fragments, a mutation, a field error and a locally refused unknown field — once with legacy
  JSON and once asking for graphql-response+json; only the four operations reach the handler.
- `http_test.rs` — Accept negotiation; schema/document/operation/variable errors, shape and
  root fields, execution over handler data (extra keys ignored, missing nullable → null, null
  propagation, abstract types without `__typename`); raw HTTP 200-vs-400, 415, 406, 405 with
  `Allow`, GET queries and refused GET mutations, `graphql_error`, partial field errors;
  handler-less introspection and the 500 category answer, introspection off; the NetGet pair
  and a client pointed at a non-GraphQL path.

`tests/client/graphql/peer_test.rs` — NetGet's client against strawberry.
