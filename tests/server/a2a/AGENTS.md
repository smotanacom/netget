# A2A tests

Peer: `python3 tests/server/a2a/install_peers.py ROOT` (a2a-sdk 1.2.1, hash-pinned wheel, with
its `http-server` extra and uvicorn 0.34.0; Python ≥ 3.10) prints `NETGET_A2A_PYTHON`.
`peer.py` uses the SDK's public API unchanged: `client URL` runs its client through card
resolution and the calls below; `server` serves an echo `AgentExecutor` and prints its port.

- `peer_test.rs` — a2a-sdk's client against NetGet's agent: card resolution, a direct message,
  a streamed task (snapshot, working, artifact, completed), GetTask, a working task and its
  cancellation, and a task-not-found error surfacing in the SDK.
- `model_test.rs` — version matching, message/part/task parsing and refusals, the stream
  sequence, card URL extraction; raw JSON-RPC POSTs refused for a missing or 0.3 version,
  parse errors, bad envelopes, unknown and push methods, unadvertised streaming and agent-role
  messages; a handler-less agent answering -32603; the NetGet pair over every client action,
  and a card pointing at another host refused.

`tests/client/a2a/peer_test.rs` — NetGet's client against a2a-sdk's agent.
