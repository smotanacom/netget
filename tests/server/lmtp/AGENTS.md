# LMTP server tests

- `wire_test.rs` drives the server over a raw socket with Python script and static handlers:
  a pipelined session (EHLO refused, MAIL before LHLO refused, the LHLO capability list,
  accepted/refused/deferred recipients, one reply per accepted recipient after DATA with the
  parsed subject and dot-unstuffed body echoed back, RSET/NOOP/VRFY/QUIT and port release on
  stop), the declared bounds (SIZE refusal, unknown MAIL parameter, an oversized body drained
  and refused per recipient, a 2000-byte command line refused, twenty refused commands
  closing with 421), the idle deadline, and the fail-closed paths: an unreachable model
  defers RCPT with 451, and a recipient the handler's answer omits gets 451, never 250.
- `real_client_test.rs` runs two independent clients, each failing when absent: CPython's
  `smtplib.LMTP` (reading the second per-recipient reply with `getreply()`) and
  `swaks --protocol LMTP`.

```bash
cargo test --no-default-features --features tcp,lmtp --test server -- lmtp:: --test-threads=4
```
