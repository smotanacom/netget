# LwM2M tests

Peers: `python3 tests/server/lwm2m/install_peers.py ROOT` downloads the Eclipse Leshan
2.0.0-M15 server and client demo jars (hash-pinned, from Maven Central) and prints
`NETGET_LWM2M_LESHAN_SERVER` and `NETGET_LWM2M_LESHAN_CLIENT`; Java 17+ must be on PATH.
`tests/helpers/lwm2m.rs` holds a server policy (accept, then read, write, read back, execute,
discover, read a missing object, observe) and a device policy (fixed values, a writable UTC
offset kept in a file). The Leshan server demo's TLS endpoint ignores `-tsp` and takes `-tp`'s
port, so it is bound to ::1 while TCP takes 127.0.0.1.

- `peer_test.rs` — the Leshan client demo registers with NetGet's server; every operation's
  result, a notification from its random temperature sensor, and deregistration on SIGTERM.
- `wire_test.rs` — codecs; NetGet's device against NetGet's server; raw registration refusals.

`tests/client/lwm2m/peer_test.rs` — NetGet's device against the Leshan server demo.
