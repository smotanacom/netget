# RCON server tests

- `wire_test.rs` drives the server with raw packets: a Source-dialect login refused then
  accepted (each preceded by srcds' empty response), command output, 5000 bytes split into
  4086 and 914, the sentinel mirrored after the output, the minecraft dialect with a
  handler-decided login, a command before login, size fields past 4096 and below 10, three
  failed logins, and the fail-closed paths (a command with no handler closes the connection;
  a login with no handler is refused).
- `real_client_test.rs` runs two independent clients, each failing when absent:
  gorcon/rcon v1.4.0's client (through `tests/client/rcon/peer`) and the Python rcon package
  2.4.9, each logging in, running commands and being refused a wrong password. Python rcon
  runs against the minecraft dialect: it reads each packet through a fresh buffered
  `makefile()`, so srcds' two-packet login answer, arriving in one read, loses the second.

```bash
cargo test --no-default-features --features tcp,rcon --test server -- rcon:: --test-threads=4
```
