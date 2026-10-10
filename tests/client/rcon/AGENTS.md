# RCON client tests

- `session_test.rs` runs the client against a scripted fixture that asserts the login (id 0,
  type 3, the password) and that each command is followed by the empty sentinel, reassembles
  output sent in two packets while skipping a stale packet, refuses an empty injected command
  before the wire, and disconnects on request; a refused password fails the connect; and a
  NetGet pair reassembles 9000 bytes from three packets.
- `real_server_test.rs` runs the client against gorcon/rcon v1.4.0's rcontest server
  through `peer/` (minecraft dialect, since rcontest answers in one packet and ignores the
  sentinel), including a refused password. It fails, never skips, without the peer.

`python3 tests/client/rcon/install_peers.py <root>` builds the Go peer (client and server
modes) and installs the Python rcon package 2.4.9 used by the server tests, printing
`NETGET_RCON_PEER` and `NETGET_RCON_PYTHON`.

```bash
cargo test --no-default-features --features tcp,rcon --test client -- rcon:: --test-threads=4
```
