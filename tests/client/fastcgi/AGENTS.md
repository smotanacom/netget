# FastCGI client tests

`peer_test.rs` runs flup 1.0.3 (independent, unchanged) and sends GET with query, header and a
custom param, a 100 KB POST, a 200 000-byte answer, a STDERR-writing request, a 404, a 302,
GET_VALUES (flup answers it, but its Python 3 lookup compares bytes with str keys, so the result
is empty) and a slow request the client aborts after one second — flup records the abort and
still completes. Needs `NETGET_FASTCGI_PYTHON` from `tests/server/fastcgi/install_peers.py`;
fails without it.
