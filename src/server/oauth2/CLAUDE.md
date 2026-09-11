# oauth2 — OAuth 2.0 authorization-server simulator

Serves the four HTTP endpoints of an OAuth 2.0 authorization server and asks the model what
each one should answer. `DevelopmentState::Experimental`, group `AI & API`, keywords
`oauth2` / `oauth` / `oauth 2.0` / `via oauth2` / `authorization server`.

**Read this first: it is a simulator, not an authorization server.** There is no signing key,
no token database, no client registry, no user directory and no TLS. An access token is a
string the model made up; the server neither remembers it nor can tell it apart from any other
string later. A `client_secret` reaches the model as text and the server checks nothing about
it. Use it to exercise OAuth2 *clients*, and as a honeypot. Never put it in front of anything
that matters.

## Files

| File | Contents |
|---|---|
| `mod.rs` | `OAuth2Server::spawn_with_llm_actions`, the four endpoint handlers, `parse_query_params`, `build_safe_response` |
| `actions.rs` | `OAuth2Protocol` (`Protocol` + `Server`), four action definitions and their executors, four `EventType` constants |

No `startup_params`: `get_startup_parameters()` is the empty default, so `StartupParams`
rejects any key a caller passes. Everything is configured through the instruction or through
`event_handlers`.

## Endpoints, events and actions

| Route | Event | Actions the event offers |
|---|---|---|
| `GET`/`POST /authorize` | `oauth2_authorize` | `oauth2_authorize_response`, `oauth2_error_response` |
| `POST /token` | `oauth2_token` | `oauth2_token_response`, `oauth2_error_response` |
| `POST /introspect` | `oauth2_introspect` | `oauth2_introspect_response`, `oauth2_error_response` |
| `POST /revoke` | `oauth2_revoke` | **none** — `.with_no_actions()` |

Anything else is a 404 with `{"error": "invalid_request"}` and no LLM call.

`call_llm` advertises `event.event_type.actions`, **not** `get_sync_actions()`, so those
per-event lists are the model's entire protocol vocabulary. They hold the real
`ActionDefinition`s (`oauth2_token_response_action()` etc.), not re-declared stubs — an
earlier version listed each action with `parameters: vec![]` and `example: json!({})`, which
told the model `oauth2_token_response` existed while never telling it the action takes an
`access_token`.

`oauth2_revoke` is the one event that deliberately offers nothing: RFC 7009 §2.2 fixes the
reply at `200` with an empty body whether or not the token existed, and `mod.rs` sends that
regardless of what the model returns. It says so with `.with_no_actions()`; left merely empty,
`call_llm` would log a BUG and trip a `debug_assert!`. The model can still note the revocation
with `append_memory`.

## Denial has to survive the round trip

Each executor tags its `ActionResult::Output` payload with an `oauth2_result` envelope key
(`"authorize"`, `"token"`, `"introspect"`, `"error"`), and `first_oauth2_payload` in `mod.rs`
dispatches on it. This is load-bearing, not tidiness.

Before the envelope existed the handlers scanned the returned JSON for a `code` field. An
`oauth2_error_response` — the model's only way to refuse — has no `code`, so it looked
identical to "the model returned nothing", and the handler fell through to a hardcoded
`AUTH_CODE_123`. **A denial was delivered to the client as a working authorization code.** The
token endpoint had the matching defect from the other direction: it returned the error body
with `200 OK`, so a conforming client parsed a refusal as a successful token response.

Three rules follow, and they are why the endpoints look the way they do:

- **`/authorize` reports errors by redirect**, per RFC 6749 §4.1.2.1 — `?error=…&state=…`
  appended to `redirect_uri`, never a code.
- **`/token` and `/introspect` answer errors with an error status** — 400 by default, 401 for
  `invalid_client` (RFC 6749 §5.2). `oauth2_error_response` takes an optional `status_code` to
  override.
- **Every endpoint fails closed.** No action, or an action belonging to a different endpoint,
  produces `server_error` / `invalid_grant` / `active: false`. The old defaults minted
  `AUTH_CODE_123`, `ACCESS_TOKEN_123` and `{"active": true, …}`, so an LLM outage silently
  issued working credentials and validated every bearer token in existence.

If you add an endpoint, tag its payload and add a `match` arm. An untagged payload falls into
the fail-closed branch, which is the safe direction.

## Hostile input

Three bounds, each of which was absent and each of which turned an attack into a *success*
rather than a refusal. `tests/server/oauth2/hardening_test.rs` covers the first two.

- **`MAX_REQUEST_BYTES` = 64 KiB.** Every endpoint is reachable before any credential is
  checked, and the body is parsed and handed to the model as prompt text, so the previous
  unbounded `req.into_body().collect()` let one anonymous POST grow the process without limit
  and drive an LLM call with megabytes of attacker-chosen prompt. Over the limit is `413`, and
  it is a refusal on every endpoint — including `/revoke`, whose RFC 7009 §2.2 blanket `200` is
  for a token the server *processed*, not one it never read.
- **`status_or` narrows a model-supplied `status_code` with `u16::try_from`.** `65736 as u16`
  is `200`, so the old `as u16` turned a refusal into the one status a client reads as "here
  is your token" — the OCI-registry truncation defect in a protocol whose success is a
  credential. Anything outside 100–599 falls back to the RFC 6749 §5.2 default.
- **`redact_params` before logging.** `client_secret`, `password`, `token`, `code` and
  `refresh_token` still reach the *model* — deciding whether they are valid is the whole job —
  but a `{:?}` of the parameter map used to copy them into `netget.log` and onto the TUI status
  stream, where they outlive the request. `/introspect` and `/revoke` now log
  `token_present=<bool>` instead of the bearer token.

## Parsing

`parse_query_params` handles `application/x-www-form-urlencoded` for both the query string and
POST bodies. It decodes `+` as space (a `scope=read+write` body used to arrive as the literal
`read+write`) and skips a pair whose key or value is not valid percent-encoding rather than
collapsing it to an empty-string key.

## Nothing here may panic

`build_safe_response` is the only place a `Response` is built. Everything fed to it —
`redirect_uri` from the client's query string, `code`/`state`/status from the model — is
untrusted: an out-of-range status becomes 500 and a header hyper rejects is dropped with a
warning.

The previous `.body(..).unwrap()` was remotely reachable. `parse_query_params`
percent-decodes, so `?redirect_uri=http://x/cb%0D%0AX-Injected:%201` produced a `Location`
value containing CRLF; hyper refused it, `.body()` returned `Err`, and the `unwrap()` killed
the connection task. Verified fixed: that request now answers 302 with the bad header dropped
and the server stays up.

`authorize_redirect` also percent-encodes every value it appends, so a `state` or `code`
containing `&` can no longer inject extra parameters into the callback URL.

This is a local copy of `http_common::handler::build_safe_response`; the `oauth2` feature
cannot reach that module because it is gated on `feature = "http"`.

## Storage

None, per the project rule. No tokens, codes, clients or sessions are kept in Rust. If a flow
needs continuity — "the code I issued a moment ago" — the model carries it in server memory
(`set_memory` / `append_memory`) or the instruction states a rule ("any code starting with
`AUTH_` is valid"). Do not add a token table here.

## Not implemented

PKCE (RFC 7636), dynamic client registration (RFC 7591), scope validation, a login page or any
user authentication, JWT access tokens, a JWKS endpoint (that is the `openid` protocol), and
TLS. `expires_in` is echoed, never enforced.

## Examples

```text
listen on port 8080 via oauth2
Accept client 'myapp' with secret 'secret123'.
Approve /authorize for that client and return an authorization code.
On /token, if the code was one you issued, return a 1-hour access token plus a refresh token.
Reject any other client with oauth2_error_response error=unauthorized_client.
```

Deterministic equivalent — no LLM call per request:

```json
"event_handlers": [
  {"event_pattern": "oauth2_authorize",
   "handler": {"type": "static", "actions": [
     {"type": "oauth2_authorize_response", "code": "AUTH_CODE_xyz123"}]}},
  {"event_pattern": "oauth2_token",
   "handler": {"type": "static", "actions": [
     {"type": "oauth2_token_response", "access_token": "ACCESS_xyz123",
      "token_type": "Bearer", "expires_in": 3600}]}}
]
```

## Tests

`tests/server/oauth2/` exists and is declared in `tests/server/mod.rs`. See
`tests/server/oauth2/CLAUDE.md`.

## References

RFC 6749 (framework), RFC 7662 (introspection), RFC 7009 (revocation), RFC 7636 (PKCE, not
implemented), RFC 7591 (dynamic registration, not implemented).
