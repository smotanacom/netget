# NETCONF server tests

Run (peers from `install_peers.py`, Python 3.10):

```bash
python3.10 tests/server/netconf/install_peers.py /abs/owned/root   # prints NETGET_NETCONF_PYTHON
NETGET_NETCONF_PYTHON=... ./cargo-isolated.sh test --no-default-features --features tcp,netconf \
    --test server --test client -- netconf:: --test-threads=100
```

- `real_client_test.rs` — **ncclient 0.7.1** (independent Python client on Paramiko 3.5.1),
  unchanged, via `peer.py client`: pinned host key, password auth, get with a subtree
  filter, get-config, edit-config, a `lock-denied` rpc-error, a candidate request refused by
  Rust from the capability list (`invalid-value`, no handler call), close-session; once with
  base:1.0 (end-of-message) and once with base:1.1 (chunked). The negotiated version is read
  back from the connection's protocol info. Fails, never skips, without the peer.
- `session_test.rs` — NetGet client ↔ server over both versions through every reply shape
  (data, ok, rpc-error with error-info, custom output, close-session), message-id
  correlation; wrong password and wrong host key never reach a session; kill-session ends
  another session and refuses itself; client-side capability refusals happen before any
  write; stopping the server with an RPC parked for a human ends the client's connection and
  retires the intercept; a peer that never completes hello is closed at
  `handshake_timeout_secs`.
- `envelope_test.rs` — what Rust refuses without a handler, attribute echo, hello
  negotiation, handler XML parsed not spliced, reply-shape and error-tag validation, client
  reply parsing and request building.
- `codec_test.rs` — framing (fragmentation, transition, exact caps, malformed headers) and
  XML (DTD/entity refusal, namespace/attribute semantics, exact depth/node/text caps).
- `owned_stream_test.rs` — dropping the owner wakes a parked read and closes the peer.

No model is involved: script and static handlers answer every event.
