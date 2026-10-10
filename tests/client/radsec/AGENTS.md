# RadSec client tests

`chain()` authenticates alice on connect and, when accepted, starts accounting for a session
named after the Access-Accept's Reply-Message (`s-<message>`). No LLM calls.

- `session_test.rs`: NetGet's own RadSec server — accept, accounting with the derived session
  id, an injected rejected user; a server whose certificate does not chain to `ca_file` is never
  connected to; no `ca_file` is refused before connecting.
- `real_server_test.rs`: **FreeRADIUS** with a TLS listener requiring a client certificate,
  and **radsecproxy** terminating TLS in front of a FreeRADIUS over UDP. In both, FreeRADIUS's
  `users` file answers "hello from freeradius", and the session id the client built from it is
  read back from FreeRADIUS's own `detail` file.

FreeRADIUS refuses TLS sockets without threading, so its TLS test runs `-f -xx -l stdout`, not
`-X`. Peers as for `tests/server/radsec`.
