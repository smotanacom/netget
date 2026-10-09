# JMAP server — Experimental

RFC 8620 (core) and RFC 8621 (mail) over hyper HTTP/1.1, HTTPS by default. The handler is the
mail store: it answers every method call except `Core/echo`. Rust owns everything a JMAP
server must decide before or around the store, in `request.rs` and `mod.rs`:

- The session resource at `/.well-known/jmap` (and `/jmap/session`): the core limits, the mail,
  submission and vacation-response capabilities, the accounts from the `accounts` parameter
  (the first primary), `apiUrl` and the other URLs built from the request's Host, and a state
  derived from the accounts.
- Authentication: Basic against `users`, Bearer against `api_tokens`; with neither configured
  every request is accepted. A refusal is 401 with `WWW-Authenticate`.
- Request-level problems (RFC 8620 §3.6.1, HTTP 400, `application/problem+json`): notJSON,
  notRequest (including nesting past 32 levels), unknownCapability, and limit for
  maxCallsInRequest (16) and maxSizeRequest (1 MiB).
- Per call: result references (`#name` with resultOf, name and an RFC 6901 path where `*` maps
  over arrays) resolved against earlier responses, `#creationId` replaced wherever an id or an
  id-keyed map appears (from `createdIds` and every earlier `created`), then method errors:
  unknownMethod (a method this server does not offer, or whose capability is not in `using`),
  accountNotFound, invalidArguments (no accountId, or both `x` and `#x`), invalidResultReference,
  requestTooLarge (256 ids per get, 128 objects per set).
- `createdIds` is returned when the request sent it; `sessionState` is always returned.

The handler gets `jmap_method_call` (method, account_id, resolved arguments, call id, username)
and answers `jmap_response` (arguments; accountId filled in when missing; a different response
name when needed) — several for one call are allowed — or `jmap_method_error`. No answer is
serverFail (`decision=model_silent`), a failed model serverFail or serverUnavailable when
overloaded (`decision=fail_closed_llm_error`).

The certificate is `tls_cert_file`/`tls_key_file`, or a self-signed one for `localhost` (30 days)
whose PEM is published as `protocol_data.certificate_pem`; `tls: false` serves plain HTTP.

Not implemented: blob upload and download, EventSource push and WebSocket (each 501), and any
storage — state strings, ids and objects are the handler's.
