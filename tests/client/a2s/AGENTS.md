# A2S client tests

- `session_test.rs` runs the client against a scripted UDP fixture asserting each request:
  a players query asks for a challenge and retries with the one it is given, a 60-player
  answer split across datagrams and delivered in reverse order is reassembled, info needs no
  challenge and its extra-data SteamID is decoded, an unknown query is refused before the
  wire; and against NetGet's own server with `info_challenge`.
- `real_server_test.rs` queries woozymasta/a2s v0.4.0's UDP server (through `peer/`) for
  info, players and rules. It fails, never skips, without the peer.

`python3 tests/client/a2s/install_peers.py <root>` builds the Go peer and installs
python-a2s 1.4.2 for the server tests, printing `NETGET_A2S_PEER` and `NETGET_A2S_PYTHON`.

```bash
cargo test --no-default-features --features tcp,a2s --test client -- a2s:: --test-threads=4
```
