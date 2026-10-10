# inetd service client tests

- `pair_test.rs` runs all six clients (Echo, Discard, Daytime, QOTD, Chargen, Time) against
  NetGet's own servers over TCP and UDP and asserts each response event: Echo's verbatim
  data and match flag, Discard's byte count, the daytime line, the quote with its line
  endings normalised, Chargen's pattern check and Time's RFC 868 decoding. It also checks
  that an echo rewritten by the server is reported as a mismatch and that an injected query
  with invalid hex is refused before the wire.
- `real_server_test.rs` runs the clients against xinetd's built-in services on TCP and UDP,
  and an external QOTD program on TCP, and checks that xinetd's clock decodes to within a
  minute of ours. It fails, never skips, without xinetd (`apt-get install xinetd`).

```bash
cargo test --no-default-features --features tcp,inetd --test client -- inetd:: --test-threads=4
```
