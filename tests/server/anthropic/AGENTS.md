# Anthropic server tests

`ASSISTANT` (in `wire_test.rs`) is the handler everything runs: it echoes the last user text and
the system prompt; the model `deny-me` is refused `rate_limit_error`, `silent` answers nothing;
a message mentioning weather with tools offered calls the first tool with `{"city":"Paris",
"days":[1,2]}`, and a `tool_result` is answered "Tool said: …". No LLM calls.

- `wire_test.rs` (raw HTTP): a plain message's shape and usage; the exact event sequence of a
  stream and that its deltas rebuild multi-byte text; a streamed tool call whose
  `input_json_delta` pieces rebuild the input; a tool_result round trip; six validation errors
  with Anthropic's messages; count_tokens, models, 404s; the API key (missing, wrong, a prefix,
  a suffix, then right by `x-api-key` and by bearer); the 8 MiB bound (+1 is 413, the bound
  itself is read); the handler's refusal, silence, no handler, and a reply the executor refuses.
- `real_client_test.rs`: the official **anthropic** Python SDK 1.9.0 (`sdk_peer.py`) and
  **@anthropic-ai/sdk** 0.129.0 (`ts_peer/peer.mjs`), retries off. Each runs a plain message, a
  stream (deltas and final message), a tool call answered with a tool_result, a streamed tool
  call, count_tokens, models list and retrieve, and four refusals that must surface as the SDK's
  own classes: `RateLimitError` 429, `BadRequestError` 400, `NotFoundError` 404 and
  `AuthenticationError` 401 (a second server with `api_key`).

Both fail rather than skip when absent. Peers: `python3 install_peers.py <root>`, which also
builds llama-server for the client suite, and prints `NETGET_ANTHROPIC_PYTHON`,
`NETGET_ANTHROPIC_TS_PEER`, `NETGET_LLAMA_SERVER` and `NETGET_LLAMA_MODEL`. CI: the
`anthropic-pairs` job in `.github/workflows/protocol-pairs.yml`.
