# Anthropic client tests

`chain_handlers(prompt)` drives the client: on connect a plain message; its text is sent back
as "again: <text>", streamed; the streamed text is counted; then the models are listed. Each
request is built from the previous answer, so the chain only completes if the client acts on
what came back. No LLM calls.

- `session_test.rs`: NetGet's own server (an echo script) — the chain, exact texts and stream
  counts; tool use plain and streamed, then a tool_result; the server's 529 and 404 refusals;
  six local refusals; disconnect. A hand-written listener checks that `x-api-key` and
  `anthropic-version` reach the wire and that the key appears in no event; no model anywhere is
  refused locally. The follow-up bound waits for the client's own "chain stopped" line and counts
  exactly 8 responses.
- `real_server_test.rs`: **llama.cpp's llama-server** b11500 with ggml-org's 260K-parameter
  stories model (`--seed 42 --verbose`). The text is a tiny model's, so it is asserted by shape:
  16 output tokens and `max_tokens`, a stream with message_start/stop and deltas, a token count,
  the model in the list. The chain's second request is found in llama-server's own
  `converted request` log line, which proves the client sent what the first answer said. Two of
  llama-server's refusals come back through its own envelope: an image block (500, "image input
  is not supported") and `/v1/models/{id}`, a route it does not have (404 `not_found_error`).

llama-server does not validate like the API (empty messages and a missing max_tokens are
accepted), so the validation assertions live against NetGet's own server.

Peers from `tests/server/anthropic/install_peers.py`.
