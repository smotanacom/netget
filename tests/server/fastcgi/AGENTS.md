# FastCGI tests

Peers: nginx (system package; `RealServer`, fails naming the package when absent) in front of
NetGet's responder; `python3 tests/server/fastcgi/install_peers.py ROOT` (flup 1.0.3,
hash-pinned wheel) prints `NETGET_FASTCGI_PYTHON` for the client's peer, `peer.py`, flup's
threaded WSGI FastCGI server with a few routes.

- `nginx_test.rs` — nginx forwards a GET with query and header, a 100 KB POST (several STDIN
  records), a 200 000-byte answer (several STDOUT records), a binary answer and a 418 whose
  STDERR line nginx writes to its error log, all on one kept-alive upstream connection; a
  handler-less responder gives nginx a 500.
- `record_test.rs` — codec (long pair lengths, malformed pairs, padding, stream splitting, CGI
  parsing/building, header checks, hex bodies); on the wire GET_VALUES, UNKNOWN_TYPE, an
  Authorizer refused, CANT_MPX_CONN, ABORT_REQUEST, KEEP_CONN and its absence, 431 and 413 bounds,
  a wrong version; the NetGet pair.

`tests/client/fastcgi/peer_test.rs` — NetGet's client against flup.
