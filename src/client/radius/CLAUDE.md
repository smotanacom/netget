# RADIUS client (NAS)

NetGet plays the NAS: it sends Access-Request (PAP or CHAP), Accounting-Request and
Status-Server to a RADIUS server, and the model decides who to authenticate and what to
account. The transport owns everything cryptographic.

## Files

- `wire.rs` — what only a client builds or checks: CHAP-Password (RFC 2865 §2.2), HMAC-MD5 and
  the Message-Authenticator (RFC 3579 §3.2) on requests, and `verify_reply`. The header, TLVs,
  User-Password hiding, the Response Authenticator and the Accounting-Request authenticator are
  the server's `src/server/radius/packet.rs`, shared rather than copied.
- `actions.rs` — vocabulary, events, `request_from_action` (every refusal before the wire).
- `mod.rs` — three tasks, all registered: transport, turns, commands.

## The shared secret

A required startup parameter, `secret`. `connect()` refuses to start without a non-empty one.
It is moved into the transport's `Settings` (which is deliberately not `Debug`) and used for
authenticators only. It is in no action, no event and no log line of this client:

- events are built from reply attributes, never from the settings;
- the generic `open_client` summary and the executor's per-action DEBUG line pass startup
  parameters and actions through `src/utils/redact.rs`, which replaces any credential-named
  key's value (`secret`, `password`, `api_key`, …) with `<redacted>` for display;
- the real-server test asserts the secret appears in no event the model was shown and in
  nothing NetGet printed.

What this cannot cover: when the **model** creates the client, the secret is in the model's own
output, and the LLM layer logs model output (a 200-byte DEBUG preview and the full body at
TRACE). A client created from the dashboard or MCP with the secret typed in never passes through
the model. A password the operator injects through `[ send ]` is replaced with `<redacted>` in
the access log.

## Every reply is verified before the model sees it

`verify_reply` checks, in order:

1. the code answers the request (Access-Request → Accept/Reject/Challenge; Accounting-Request →
   Accounting-Response; Status-Server → Access-Accept or Accounting-Response);
2. the **Response Authenticator**, `MD5(Code|ID|Length|RequestAuth|Attributes|Secret)`;
3. the **Message-Authenticator**, HMAC-MD5 over the reply with the Request Authenticator in the
   authenticator field — and on replies to Access-Request and Status-Server it is **required**:
   a reply without one is refused (`missing_message_authenticator`), the BlastRADIUS
   (CVE-2024-3596) mitigation. Every Access-Request and Status-Server this client sends carries
   one, so a server configured `require_message_authenticator = yes` (the real-server test's
   FreeRADIUS is) accepts it.

A reply that fails 1-3 is discarded (RFC 2865 §3), reported as `radius_error {kind}`, and the
request **goes on waiting** — a forged reject cannot cancel a genuine accept that is still on its
way. A reply from an address the request did not go to is dropped before it is read
(`decision=unexpected_source`); a datagram over 4096 bytes is refused by the shared decoder.

## Reliability

`timeout_ms` (default 3000, 100-60000) and `retries` (default 2, 0-5), declared with defaults
from `DEFAULT_TIMEOUT_MS` / `DEFAULT_RETRIES`. A retransmission is the identical packet — same
identifier, same authenticator (RFC 2865 §2.5). After the last, `radius_error {kind: timeout}`.
Identifiers are allocated from a random start, skipping any in flight; at most 32
(`MAX_IN_FLIGHT`) requests wait at once.

## Events

`radius_connected {auth_server, accounting_server}`,
`radius_access_accept | radius_access_reject | radius_access_challenge {user_name, method,
reply_message?, attributes}`, `radius_accounting_response {status_type, session_id,
attributes}`, `radius_status_response {port, code, attributes}`,
`radius_error {kind, message, request, user_name?}`.

`attributes` is every reply attribute by dictionary name (repeated ones as arrays), minus
Message-Authenticator, State and Proxy-State, which mean something only to the transport.
Opaque values (Class) are hex, as the server reports them.

## Actions

`radius_access_request {user_name, password?, method?: pap|chap, nas_identifier?, attributes?}`,
`radius_accounting_request {status_type, session_id, user_name?, nas_identifier?, attributes?}`,
`radius_status_server {port?: auth|accounting}`, `disconnect`.

`attributes` take dictionary names with typed values (integers, dotted quads, text). Refused:
User-Password, CHAP-Password, Message-Authenticator, State, Proxy-State and EAP-Message (the
client sets these itself, and saying so is the refusal's message), any octet-string attribute,
integers past 32 bits, unknown names, passwords past 128 bytes.

**Challenges.** An Access-Challenge's State is remembered per user (at most 64) and carried on
that user's next Access-Request automatically, so the model answers a challenge by sending the
user's response as `password`. This path is not exercised against a real server.

## Ports

`remote_addr` is the authentication server; accounting goes to `accounting_port` on the same
host, default the authentication port plus one (1812 → 1813).

## Limitations

No EAP, MS-CHAP, CoA/Disconnect-Request (RFC 5176), RadSec, or IPv6 attributes; one server.

## Maturity

Evidence: `tests/client/radius/real_server_test.rs` against FreeRADIUS `radiusd`. See
`tests/client/radius/CLAUDE.md`.
