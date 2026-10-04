# SCIM tests

Peers: `python3 tests/server/scim/install_peers.py ROOT` (scim2-tester 0.5.1, scim2-cli 0.4.0,
scim2-server 0.4.0; hash-pinned wheels; Python ≥ 3.11) prints `NETGET_SCIM_BIN`.
`tests/helpers/scim.rs` holds a complete script store (create with ids and userName uniqueness,
replace, PATCH add/replace/remove on attributes, sub-attributes, extensions and `eq` value
filters, delete; list returns everything and leaves the query to NetGet).

- `peer_test.rs` — scim2-tester builds its models from NetGet's discovery and passes every
  check (discovery, CRUD, PATCH add/replace/remove on every attribute, attribute selection on
  get, list and `/.search`) with no ERROR; scim2-cli creates three users, runs a compound filter
  with a value path sorted descending, a case-insensitive givenName filter, attribute selection,
  a 409 uniqueness conflict, a replace and a delete.
- `http_test.rs` — filter evaluation, PATCH paths, projection and sorting; bearer auth,
  discovery, invalidFilter/invalidSyntax/noTarget/invalidPath, paging with sortBy and
  attributes, root `/.search` with count 0, a value-filter PATCH, 501, 404; a handler-less
  service (500 for data, discovery still served); the NetGet pair.

`tests/client/scim/peer_test.rs` — NetGet's client against scim2-server.
