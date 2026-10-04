# ICAP tests

Peer: `bash tests/server/icap/install_peers.sh ROOT` builds c-icap 0.6.5 from its pinned
release tarball into an owned prefix and prints `NETGET_C_ICAP`.

- `peer_test.rs` — c-icap-client (independent) against NetGet's server: OPTIONS (Methods,
  Preview, ISTag), a clean RESPMOD, an EICAR RESPMOD answered with the 403 page, a POST
  REQMOD redacted to `[redacted]` with `X-Redacted`, and a 3000-byte body sent with a
  512-byte preview that must be continued and come back whole.
- `wire_test.rs` — Encapsulated rules, chunk bounds and `ieof`, header injection refusal,
  the server's 404/505/405/400 answers, a handler-less server answering 500 (never 204), and
  the NetGet pair through every verdict and a continued preview, plus client-side refusals.

`tests/client/icap/peer_test.rs` — NetGet's client against c-icap's echo service: OPTIONS, an
echoed RESPMOD, a REQMOD (204 or echoed), and a continued 3000-byte preview.
