# tests/client/bitcoin

Bitcoin RPC **client** (JSON-RPC over HTTP — not the P2P wire protocol, which is
`src/server/bitcoin`). No `bitcoind` and no regtest node anywhere in this suite: the peer is
either a NetGet HTTP server or a hand-written HTTP responder. That proves the client speaks
JSON-RPC to *something*, not that Bitcoin Core would accept it, which is why
`src/client/bitcoin` is `DevelopmentState::Experimental`.

## `e2e_test.rs` — two tests, **10 LLM calls**

Each test spawns two NetGet subprocesses (a server and a client), each with its own mock
Ollama, and each with five `.expect_calls(1)` rules — 2 on the server, 3 on the client.

| test | server rules | client rules |
|---|---|---|
| `test_bitcoin_client_connection` | startup, `http_request` (POST) → JSON-RPC body | startup, `bitcoin_connected` → `get_blockchain_info`, `bitcoin_response_received` → nothing |
| `test_bitcoin_client_rpc_command` | same | same |

Both tests stand up a NetGet **HTTP** server as the RPC node (`"base_stack": "HTTP"`), because
Bitcoin Core's RPC is JSON-RPC over HTTP and there is no Bitcoin *server* protocol to point at.
`bitcoin = ["dep:bitcoin"]` pulls in no such thing, so `tests/client/bitcoin/mod.rs` gates this
file on `all(feature = "bitcoin", feature = "http")`. At `--features bitcoin` alone it used to
compile and then fail at runtime with `Protocol 'HTTP' exists but is not compiled into this
build` — which reads exactly like the `target/` contention artefact the root CLAUDE.md warns
about, and is not. **This is why the run command below needs both features.**

## `command_channel_test.rs` — one test, **0 LLM calls**

`injected_bitcoin_rpc_reaches_the_node`. In-process, no NetGet subprocess; the client's LLM
points at `http://127.0.0.1:1`, so its `bitcoin_connected` call fails and the connect path has
to tolerate that — verifying it does is part of the test. The peer is a ~40-line HTTP/1.1
responder bound to 127.0.0.1 that records the JSON-RPC bodies it receives, so this file needs
only `bitcoin`.

What it pins:

- `has_client_handle` is true **before** anything answers the connected event — the regression
  guard for "register the channel first".
- `get_blockchain_info` → `Executed { detail }` naming the method and HTTP status, and the stub
  really received `getblockchaininfo`. `Executed` rather than `Sent` is deliberate: reqwest
  reports no byte count, so a `Sent` here would be invented.
- An unknown action → `Rejected`, not silence.
- `disconnect` → `Disconnected`, then status `Disconnected` and the handle gone.

## Running

```bash
# both features: e2e_test needs the HTTP server protocol
./cargo-isolated.sh test --no-default-features --features bitcoin,http \
    --test client -- client::bitcoin --test-threads=100
```

All mocked; no Ollama required.
