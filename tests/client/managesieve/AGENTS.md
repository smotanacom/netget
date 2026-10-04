# ManageSieve client tests

`peer_test.rs` runs Dovecot 2.4.5 with Pigeonhole 2.4.5 (independent, unchanged) unprivileged,
with any user accepted on password "secret". Pigeonhole compiles every script it is given, so
the refused upload carries its real compiler error. NetGet's client uploads, checks, activates,
lists, reads, renames and asks for space; deleting the active script and reading a missing one
are refused with ACTIVE and NONEXISTENT; the renamed script and Dovecot's active link are read
back from Dovecot's own files; a wrong password fails the connect. Needs
`NETGET_MANAGESIEVE_DOVECOT` from `tests/server/managesieve/install_peers.py`.
