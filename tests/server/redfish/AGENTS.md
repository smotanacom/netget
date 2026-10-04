# Redfish tests

Peers: `python3 tests/server/redfish/install_peers.py ROOT` builds `gofish/` (a program on
gofish v0.26.0's public API; `go.sum` pins the module, `-mod=readonly`), installs DMTF
redfishtool 1.1.8 (hash-pinned wheel) and unpacks DMTF Redfish-Mockup-Server 1.3.0
(hash-pinned tarball) with its dependencies, printing the `NETGET_REDFISH_*` variables.
`tests/helpers/redfish.rs` holds a BMC policy whose AssetTag PATCH persists in a temp file.

- `peer_test.rs` — gofish: a refused wrong password, session login, the service root, systems
  (typed: power, model, CPU and memory summaries, allowable reset types), a reset returning a
  task monitor it polls to Completed, a PATCH read back, chassis sensors, managers, sessions,
  logout. redfishtool: Basic auth read, a reset it waits on, session auth reads through `raw` and
  `Managers`, a refused wrong password.
- `http_test.rs` — envelope rules; the unauthenticated documents, 401 with `WWW-Authenticate`,
  session create/list/delete, Basic auth remembered, handler 404, an invalid resource failing
  closed, 415, PropertyNotWritable, ActionParameterMissing / ValueNotInList, a task monitor
  202 → 204 and its Task, create (201 + Location) and delete, 405, 412; a handler-less service
  refusing logins and answering 500; the NetGet pair.

`tests/client/redfish/peer_test.rs` — NetGet's client against the DMTF mockup server.
