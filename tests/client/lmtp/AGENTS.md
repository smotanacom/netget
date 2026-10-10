# LMTP client tests

- `session_test.rs` runs the client against a scripted fixture that asserts every byte it
  receives: LHLO with the configured `lhlo_domain`, MAIL/RCPT/DATA for the handler's
  `lmtp_send`, dot-stuffing, the generated Date/Message-ID/From/To headers, one delivery
  reply read per accepted recipient (written in fragments), RSET after a transaction no
  recipient accepted, an injected send refused before the wire, injected QUIT, a malformed
  reply ending the session, and a connect refused when the server will not take LHLO.
- `real_server_test.rs` delivers through aiosmtpd 1.4.6's `LMTP` class (`peer.py`), an
  independent Python implementation, and asserts the envelope and content aiosmtpd recorded
  as well as the per-recipient results NetGet reported. It fails, never skips, without
  aiosmtpd: `python3 tests/client/lmtp/install_peers.py <root>` installs the hash-pinned peer
  and prints the `NETGET_LMTP_PYTHON` export.

No test consults a model: handlers are static rules, so condition 4 (the client acts on the
handler's answer) is asserted from the server's side of the wire.

```bash
cargo test --no-default-features --features tcp,lmtp --test client -- lmtp:: --test-threads=4
```
