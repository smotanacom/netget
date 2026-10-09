# GraphQL client tests

`peer_test.rs` runs strawberry-graphql 0.330.2 (independent, unchanged) under uvicorn and checks
that NetGet's client introspects its root fields, then runs a POST query with variables, a GET
query, a union with fragments, a mutation, a resolver error (data null, error path) and a
validation error strawberry refuses; a GET mutation, a subscription and a syntax error are
refused before sending. Over graphql-transport-ws: a countdown with variables that completes,
an endless subscription it cancels (a second cancel is refused), and a subscription strawberry
refuses; a query sent to graphql_subscribe is refused locally. Needs `NETGET_GRAPHQL_PYTHON` from `tests/server/graphql/install_peers.py`;
fails without it.
