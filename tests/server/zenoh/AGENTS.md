# Zenoh tests

Peers: `python3 tests/server/zenoh/install_peers.py ROOT` builds zenoh-pico 1.10.1 (a C
implementation of Zenoh, independent of the Rust runtime NetGet embeds) from its hash-pinned
release tarball with examples, and prints `NETGET_ZENOH_PICO` (the examples directory). The
examples block-buffer stdout on a pipe, so the tests run them to completion (`-n`) and read the
output afterwards.

- `peer_test.rs` — pico clients against NetGet's router: a publication the handler echoes to a
  pico subscriber, a query answered and one refused with an error, and a get the handler issues
  answered by a pico queryable; the links appear as loopback connections.
- `wire_test.rs` — action validation; NetGet's client against NetGet's router; a handler with an
  invalid answer failing the query closed with a category only; a bad startup key refused.

`tests/client/zenoh/peer_test.rs` — NetGet's client against pico peers.
