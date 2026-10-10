# Anthropic Messages API server

Anthropic's Messages API over hyper (HTTP/1.1, keep-alive, at most 1000 requests per
connection), in the shapes the official SDKs parse. The handler writes every assistant message;
Rust owns everything else. There is no well-known port: the real API is HTTPS on 443, which this
plain-HTTP server does not speak, so it is in `NO_WELL_KNOWN_PORT`.

## Endpoints

- `POST /v1/messages`: validated the way the API validates (`model`, `max_tokens` >= 1,
  non-empty `messages` with `user`/`assistant` roles, string or block content), answered as a
  `message` object, or with `stream: true` as server-sent events: `message_start` (empty
  content, null stop reason), `ping`, then per block `content_block_start` /
  `content_block_delta` (`text_delta`, or `input_json_delta` for a tool call's input) /
  `content_block_stop`, then `message_delta` (stop reason, output tokens) and `message_stop`.
- `POST /v1/messages/count_tokens`: the same validation without `max_tokens`; the count is an
  **estimate** (characters / 4), not a tokenizer's.
- `GET /v1/models`, `GET /v1/models/{id}`: from the `models` startup parameter.
- Errors use Anthropic's envelope, `{"type":"error","error":{"type","message"},"request_id"}`,
  with the status each type implies (400, 401, 403, 404, 413, 429, 500, 529), and every response
  carries a `request-id: req_…` header.

## What the handler sees and decides

`anthropic_message {model, max_tokens, system, messages, tools, tool_choice, temperature, top_p,
top_k, stop_sequences, thinking, user_id, stream, api_key_present}`. Messages are summarised
block by block: text, `tool_use {id, name, input}`, `tool_result {tool_use_id, content as text,
is_error}`, and `image`/`document` described by source type, media type and approximate size —
never their base64.

It answers `anthropic_reply {text | content [text, tool_use {name, input, id?}], stop_reason?,
stop_sequence?, input_tokens?, output_tokens?}` (Rust adds `msg_`/`toolu_` ids, the model and
estimated usage; stop reason defaults to `tool_use` when a tool is called, else `end_turn`) or
`anthropic_error {error_type, message}`.

## API key

With the `api_key` startup parameter set, a request must carry it in `x-api-key` or as a bearer
token; any other is answered 401 `authentication_error` **without asking the handler**,
compared in constant time. Without it, the handler sees only `api_key_present`, never a value.

## Failure modes and bounds

- Handler failure: 529 `overloaded_error` when the backend is at capacity, else 500
  `api_error`, with `WireFailure`'s text (`fail_closed_llm_error`, `fail_closed_invalid_reply`).
  Silence is 500 too (`model_silent`); a model refusal is `model_reject`. Never a fabricated
  message.
- The reply is computed before anything is sent, so a stream is the whole message cut into
  deltas of about 16 bytes, and an error is always an HTTP status rather than a mid-stream
  `error` event.
- Bodies over 8 MiB: 413 `request_too_large` (Anthropic's own limit is 32 MB); 1000 messages,
  1000 blocks per message, 256 tools; replies at most 64 blocks and 1 MiB of text; 64 KiB /
  100 headers; 30 s for headers, 60 s for the body.
- Not implemented: batches, files, prompt caching, extended thinking output, citations, server
  tools.

## Tests

`tests/server/anthropic/`: raw HTTP (`wire_test.rs`) and both official SDKs
(`real_client_test.rs`). See its AGENTS.md.
