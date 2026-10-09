# EPP tests

Peers: `python3 tests/server/epp/install_peers.py ROOT` installs pyepp 0.2.0 with
`requirements.txt` (`--require-hashes`) and builds `registry/` (Go 1.23+, go.sum-pinned), and
prints `NETGET_EPP_PYTHON` and `NETGET_EPP_REGISTRY`. `tests/helpers/epp.rs` holds a registry
handler (taken.example belongs to OtherReg; everything else is free) and raw framing.

`registry/main.go` is the server NetGet's client is tested against: the Swedish Internet
Foundation's epp-lib v0.2.0 (unchanged) provides TLS, framing, the greeting call and command
routing by namespace URI; the file itself holds a small in-memory registry (login, check,
contact/host/domain create, info, renew with the expiry check, transfer request and query).

- `peer_test.rs` — pyepp against NetGet's server over TLS: greeting, 2002 before login, a refused
  and an accepted login, check, contact, host and domain creates, info, a missing domain (2303),
  renew, a transfer accepted (1001) and refused (2202), logout; the fields the handler saw.
- `wire_test.rs` — raw frames over plain TCP: hello, the session rules, 2100, 2307, 2000, 2001
  (malformed and DOCTYPE), 2005, poll, a handler answer refused (2400), an oversized frame (2500
  and close), three failed logins (2501 and close), a handler-less server (2400).

`tests/client/epp/`: `peer_test.rs` drives NetGet's client through a whole provisioning flow
against the epp-lib registry and refuses a wrong password and an untrusted certificate;
`pair_test.rs` pairs it with NetGet's server, with injected commands.
