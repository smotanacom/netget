# Anthropic Messages API client

Talks to an Anthropic-compatible server over plain HTTP/1.1 (hyper, one connection per request,
`x-api-key` and `anthropic-version` sent). Plain HTTP only: it reaches a local or proxied server,
not `api.anthropic.com` directly. The request and response shapes are the server's
(`crate::server::anthropic::wire`).

## Actions

- `anthropic_create_message {prompt | messages, system?, model?, max_tokens? (1024), tools?,
  tool_choice?, temperature?, stop_sequences?, stream?}`: a streamed answer is **reassembled**
  into one message — text deltas joined, `input_json_delta` pieces parsed into the tool input at
  `content_block_stop`, stop reason and output tokens from `message_delta` — and the event also
  counts each event type seen. An `error` event mid-stream fails the request.
- `anthropic_count_tokens`, `anthropic_list_models`, `anthropic_get_model {model_id}`,
  `disconnect`.

Every action raises `anthropic_response {operation, status, message? {id, model, text,
content, stop_reason, stop_sequence, usage}, streamed?, input_tokens?, models?, error? {type,
message}}`. A refusal is reported from the server's error envelope, whichever shape it is
(Anthropic's, or llama.cpp's with a `code`).

Refused locally before anything is sent: no model (neither the action nor the `model` startup
parameter), prompt and messages both or neither, a role other than user/assistant, temperature
outside 0..1, max_tokens 0, a model id with characters outside `[A-Za-z0-9-_.:@]` (it is a
path segment).

## Bounds and limits

Responses: 8 MiB, 120 s (generation is slow), 100 000 stream events, 64 blocks and 1 MiB of
streamed text. A handler chain stops after `MAX_FOLLOWUP_DEPTH` (8). `api_key` is a startup
parameter, so the redactor masks it; it is never put in an event.

## Found by the tests

The session first ran its request check on `disconnect`, which has no request, so a disconnect
was refused. The session also recorded each response in the access log itself, which
`call_llm_for_client` already does — every entry appeared twice. Eighteen older clients still
record twice; that is a separate fix.

## Tests

`tests/client/anthropic/`: NetGet's own server and llama.cpp's llama-server. See its AGENTS.md.
