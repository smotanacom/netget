# ManageSieve tests

Peers: `python3 tests/server/managesieve/install_peers.py ROOT` builds Dovecot 2.4.5 and
Pigeonhole 2.4.5 from hash-pinned tarballs (one version, so one configuration, on macOS and
Linux) and installs sievelib 1.5.0 from a hash-pinned wheel; it prints
`NETGET_MANAGESIEVE_DOVECOT` and `NETGET_MANAGESIEVE_PYTHON`. `peer.py` drives sievelib;
`tests/helpers/managesieve.rs` holds the script-keeping policy and the unprivileged Dovecot
configuration (runtime directory under /tmp so its sockets fit sun_path; `default_vsz_limit =
1024G` because macOS refuses Dovecot's default data limit; no chroot for login services).

- `peer_test.rs` — sievelib logs in and uploads, checks, activates, lists, reads, renames,
  deletes and asks for space, with the policy's refusals (a script error, NONEXISTENT, ACTIVE,
  ALREADYEXISTS, QUOTA/MAXSIZE) and a refused login. sievelib normalizes the script it reads to
  LF lines, so exact bytes are checked on the server's side.
- `wire_test.rs` — the grammar; NetGet's client against NetGet's server; raw refusals
  (unauthenticated commands, NOOP tag, continuation AUTHENTICATE, a bad name, an oversized
  literal, three failed logins).

`tests/client/managesieve/peer_test.rs` — NetGet's client against Dovecot/Pigeonhole.
