# WebTransport tests

Peers: `NETGET_AIOQUIC_PYTHON` names a Python with `tests/helpers/aioquic-requirements.txt`
(aioquic 1.3.0, unchanged); `peer.py` drives it as a WebTransport client (`client PORT CAFILE
SCENARIO`), as an echo server (`server DIR`), and writes ECDSA certificates (`cert DIR`).
aioquic's H3 layer does not mark the bidirectional WebTransport streams it opens itself, so
`peer.py` reads the answers on those streams at the QUIC layer; the library is not patched.
`NETGET_CHROME` names Chrome or Chromium and `node` (22+, for its WebSocket) must be on PATH for
`browser.mjs`, which drives the page's own WebTransport API over the DevTools protocol.
`tests/helpers/webtransport.rs` holds the handler policy both server suites use.

- `peer_test.rs` — aioquic against NetGet's server with a CA it trusts: admission with an
  extra response header, bidirectional and unidirectional streams, a datagram, a stream the
  server opens and aioquic answers (raised as `webtransport_stream_reply`), hex data, a
  1 MiB + 1 stream reset without reaching the handler, an unanswered stream finished empty, the
  handler closing with code 7; 403, 429 and 404 refusals, a plain HTTP/3 GET, and a server with
  no handler refusing with 429.
- `browser_test.rs` — headless Chrome against the self-signed certificate by its published
  hash: a stream, a unidirectional stream answered on a server-opened one, a datagram and a
  refused path.

`tests/client/webtransport/`: `peer_test.rs` runs NetGet's client against aioquic's echo server
(pinned certificate, extra headers, both stream kinds, datagrams, answering a server-opened
stream, an injected datagram, the handler closing; a refused path, a wrong pin, conflicting
trust); `pair_test.rs` pairs NetGet's client with NetGet's server through the published hash,
injects on both sides and checks that a client close ends the server's connection.

`tests/vendored_wtransport_patch_test.rs` pins the vendored copies and decodes hostile QPACK
field sections and SETTINGS frames.
