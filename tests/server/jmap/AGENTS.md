# JMAP tests

Peers: `python3 tests/server/jmap/install_peers.py ROOT` installs the Stalwart 0.16.24 release
binary (SHA-256 pinned per platform) and jmapc 0.3.0 with `requirements.txt` (`--require-hashes`)
and prints `NETGET_JMAP_STALWART` and `NETGET_JMAP_PYTHON`. `tests/helpers/jmap.rs` holds a
small store handler (two mailboxes, two emails, a create, changes since `s1`).

Stalwart 0.16 keeps listeners and accounts in its store, settable only through its JMAP
registry API (`urn:stalwart:jmap`). `Stalwart::start` provisions a fresh store in recovery mode —
whose one listener binds `[::]` on the port given, with no option to narrow it — on a random
port with a one-time admin password: an HTTP listener on 127.0.0.1, the domain and account,
DNS resolved against 127.0.0.1 and spam-rule updates off. It then restarts Stalwart, which
binds 127.0.0.1 only and reaches nothing.

- `peer_test.rs` — jmapc against NetGet's server over HTTPS (the published certificate): session,
  Core/echo, mailbox query → get by reference, email query → get, a create read back by creation
  id, an update refused for an unknown id, changes and a cannotCalculateChanges error, and a
  wrong password (401).
- `wire_test.rs` — raw HTTP against a plain-HTTP server: the session, Basic, Bearer and refused
  credentials, every request-level problem, the method errors Rust decides, createdIds both
  ways, 501 for blobs and push, 405, and a handler-less server answering serverFail.

`tests/client/jmap/`: `peer_test.rs` drives NetGet's client against Stalwart through a create,
a get by creation id, a query → get by reference, changes, a keyword update and an unknown
method, then asks Stalwart directly what it holds, and refuses a wrong password;
`pair_test.rs` pairs it with NetGet's server over HTTPS with a Bearer token and an injected
request.
