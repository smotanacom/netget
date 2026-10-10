# inetd service server tests

- `wire_test.rs` exercises all six servers over raw TCP and UDP: a verbatim echo of text and
  binary data, an echo rewritten by a script, UDP-only transport binding no TCP listener,
  Discard closing at its byte limit and closing a refused connection, Daytime with handler text and
  with the server clock, QOTD line endings, Time's RFC 868 value for an RFC 3339 answer,
  Chargen's exact rotating pattern, a custom charset, its byte limit, and a 2 MB stream that
  continues after the client half-closes (the defect netcat found), oversized datagrams
  dropped, silence after a deliberate `<service>_refuse`, and silence from every service when
  the handler fails.
- `real_client_test.rs` runs independent clients, each failing when absent: Perl's Net::Ping
  in udp and stream modes (with a control proving it checks the echoed bytes), Perl's
  Net::Time for Daytime and Time over TCP and UDP, rdate over TCP and UDP, and netcat for
  QOTD and a Chargen stream checked against RFC 864.

`tests/helpers/inetd.rs` starts the servers for both suites.

```bash
cargo test --no-default-features --features tcp,inetd --test server -- inetd:: --test-threads=4
```
