# RESTCONF tests

Peers: `python3 tests/server/restconf/install_peers.py ROOT` builds `fc/` — a small Go program
over the unchanged FreeCONF restconf and yang modules (pinned by go.sum) that serves FreeCONF's
car example over RESTCONF (`fc serve PORT`) or runs FreeCONF's client against a URL
(`fc client URL`, one JSON line per step); it prints `NETGET_RESTCONF_FREECONF`.
`tests/helpers/restconf.rs` holds a car datastore policy that answers in RFC 8040 shapes.

- `peer_test.rs` — FreeCONF's client against NetGet's server: module list, reads, a PATCH read
  back, car:addOil. FreeCONF's client expects list entries unwrapped (RFC 8040 wraps them) and
  resolves keys lazily; both are recorded in the file header, not asserted as successes.
- `wire_test.rs` — paths; NetGet's client against NetGet's server (RFC-shaped list entry, 201
  with Location, 409 data-exists, 204, a 404 error document, operation output); raw HTTP
  (host-meta XRD and JSON, 406, 415, query refusal, OPTIONS, HEAD, operations list, 405).

`tests/client/restconf/peer_test.rs` — NetGet's client against FreeCONF's server.
