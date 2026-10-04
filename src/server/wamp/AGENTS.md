# WAMP router — Experimental

WAMP v2 basic profile over WebSocket (tokio-tungstenite) with the `wamp.2.json` subprotocol; a
handshake that does not offer it is refused with 400. `uri.rs` holds message codes, URI and ID
rules and pattern matching.

Rust owns:
- HELLO first, within `hello_timeout_secs`, as `[1, realm, details]` with at least one of the
  four client roles; anything else is ABORT `wamp.error.protocol_violation` / `no_such_role`.
- Realms (created on first admitted session), session IDs, and per realm: subscriptions (exact,
  prefix, wildcard; `details.topic` on pattern matches; publisher exclusion, `exclude` and
  `eligible` lists, `disclose_me`), single-callee exact registrations, and calls routed as
  INVOCATION → YIELD → RESULT or ERROR → ERROR. A callee that leaves fails its pending calls
  with `wamp.error.canceled`. Refusals: `no_such_subscription`, `no_such_registration`,
  `procedure_already_exists`, `invalid_uri`, `option_not_allowed`. GOODBYE is answered with
  `wamp.close.goodbye_and_out`; any malformed message aborts.
- Bounds: `max_message_bytes` (1 MiB) per frame and message, JSON depth 64, 1024 subscriptions
  and registrations per realm, a 1024-message outbox per session.

The handler answers `wamp_hello` (`wamp_welcome` with an optional authrole, or `wamp_abort`) and
`wamp_call` — a CALL to a procedure no session registered — with `wamp_result` or `wamp_error`.
No admission decision is ABORT `wamp.error.not_authorized`; no call answer is ERROR
`wamp.error.unavailable`. Publications between sessions never reach the handler. Each session's
peer handle accepts `wamp_publish` (an event from the router into that session's realm) and
`disconnect`.

Not implemented: MessagePack/CBOR serializers, RawSocket, authentication beyond anonymous,
progressive results, call cancellation (CANCEL is ignored), shared registrations, the meta API,
event history.
