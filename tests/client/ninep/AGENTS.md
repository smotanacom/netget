# 9P client tests

- `session_test.rs`: every action against NetGet's own server (the server's tree script is
  copied in, since the test binaries share no modules), refusals reported as results, a
  19-element path walked in two steps, hex writes and appends seen by the server's handler,
  and a bad name rejected before anything is sent.
- `real_server_test.rs`: knusbaum/go9p's in-memory file server, unchanged, through `peer/`:
  reads, a create + write + append read back from what the independent server stored, mkdir,
  remove.

`install_peers.py ROOT` builds `peer/` (go.sum-pinned 9fans.net/go and knusbaum/go9p) and
prints NETGET_NINEP_PEER, which both this suite and `tests/server/ninep` use.
