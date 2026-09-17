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

## Failure behaviour

Every terminal outcome carries a `decision=` token on the status stream and in `netget.log`,
because **nothing on the wire distinguishes them**: a 500 the model chose with
`oauth2_error_response` and a 500 the server fell back to are the same bytes. `grep
decision=fail_closed` is the diagnostic; it only works if the tag is there.

| Outcome | On the wire | Log |
|---|---|---|
| `/authorize` — model returned `oauth2_authorize_response` with a `code` | `302` to `redirect_uri` with `code=` | INFO `decision=model_answer` |
| `/authorize` — model returned `oauth2_error_response` | `302` with `error=`, no code | INFO `decision=model_reject` |
| `/authorize` — authorize action carried no `code`, or a payload belonging to another endpoint | `302` with `error=server_error` | ERROR `decision=fail_closed_bad_action` |
| `/authorize` — no action at all | `302` with `error=server_error` | WARN `decision=model_silent` |
| `/authorize` — `call_llm` failed | `500`/`503` + RFC 6749 §5.2 code, category text only | ERROR `decision=fail_closed_llm_error` / `..._llm_overloaded` |
| `/token` — model returned `oauth2_token_response` | `200` + the token | INFO `decision=model_answer` |
| `/token` — model returned `oauth2_error_response` | its `status_code`, default `400` | INFO `decision=model_reject` |
| `/token` — payload for another endpoint | `400 invalid_grant` | ERROR `decision=fail_closed_bad_action` |
| `/token` — no action | `400 invalid_grant` | WARN `decision=model_silent` |
| `/token` — `call_llm` failed | `500`/`503`, **never** `invalid_grant` | ERROR `decision=fail_closed_llm_error` / `..._llm_overloaded` |
| `/introspect` — model returned `oauth2_introspect_response` | `200` + its verdict (`active` either way) | INFO `decision=model_answer` |
| `/introspect` — model returned `oauth2_error_response` | its `status_code`, default `400` | INFO `decision=model_reject` |
| `/introspect` — payload for another endpoint | `200 {"active": false}` | ERROR `decision=fail_closed_bad_action` |
| `/introspect` — no action | `200 {"active": false}` | WARN `decision=model_silent` |
| `/introspect` — `call_llm` failed | `500`/`503`, **not** `{"active": false}` | ERROR `decision=fail_closed_llm_error` / `..._llm_overloaded` |
| `/revoke` — `call_llm` succeeded | `200`, empty body | INFO `decision=protocol_mandated_ok` |
| `/revoke` — `call_llm` failed | `500`/`503` | ERROR `decision=fail_closed_llm_error` / `..._llm_overloaded` |
| any endpoint — body over `MAX_REQUEST_BYTES` | `413`, no LLM call | WARN `decision=refused_body_too_large` |
| unrouted path | `404`, no LLM call | DEBUG `decision=unknown_endpoint` |

Two tokens here are not from the shared vocabulary, and both name a decision the **protocol**
made rather than the model:

- `decision=protocol_mandated_ok` — `/revoke`'s success path. RFC 7009 §2.2 fixes the reply at
  `200` whether or not the token existed, which is why `OAUTH2_REVOKE_EVENT` declares
  `.with_no_actions()`. The model is asked (so an outage is still detectable) but its answer
  changes nothing, so calling this `model_answer` would credit a decision nobody made. Note
  that the `200` asserts less than it looks like: this server stores no tokens, so nothing was
  revoked in any case.
- `decision=refused_body_too_large` — already in use elsewhere in the tree, emitted from
  `read_body_limited`, which is shared by all four endpoints and so does not name one.

**`fail_closed_llm_overloaded`** is emitted where `WireFailure::classify` says `Overloaded`;
it keeps the `fail_closed_` prefix so the documented grep still finds it, while telling an
operator the backend was saturated rather than dead.

### The historical fail-open is gone — verified, not assumed

The root `CLAUDE.md` names this protocol as *the* worked example of a fail-open: no action
meant a hardcoded `AUTH_CODE_123`, a hardcoded `ACCESS_TOKEN_123`, and introspection answering
`{"active": true}` for every bearer token. **That is history.** Re-read against the current
`mod.rs` (September 2026): every one of the four endpoints refuses when the model produces
nothing usable, and no branch of any of them can synthesise a credential — there is no
literal token, code or `active: true` anywhere in the file. The section above is derived from
that reading rather than from the older prose, and `tests/server/oauth2/llm_failure_test.rs`
pins all four.

Two things that are *not* fail-opens but look adjacent, recorded so the next reader does not
re-flag them:

- `/introspect` answering `200 {"active": false}` when the model said nothing is affirmative
  in shape but negative in content, and a resource server refuses on it. The **outage** path
  deliberately does not use it, because it is a statement that the server looked.
- An `oauth2_error_response` may carry `status_code: 200`. That is the model explicitly
  choosing a status, tagged `model_reject`, and `status_or` only stops it wrapping out of
  range — it does not second-guess an in-range choice.

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

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever,
pre-authentication, and a hundred of them was a free denial of service on a server that would
happily accept a hundred more. It now declares both halves; the constants and the argument for
each live beside them in `src/server/oauth2/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 30s | HTTP is client-speaks-first, so a peer that has completed the handshake and sent no byte has asked nothing and negotiated nothing — the state carries no protocol yet, which is why this number is the same across netget's HTTP family. Apache's `mod_reqtimeout` gives the request header 20s and nginx's `client_header_timeout` 60s. Enforced with `TcpStream::peek` before the socket reaches hyper, so the request line is still there afterwards. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 60s | These endpoints are reached two ways and the two pull in opposite directions: a **browser** does one redirect round and never comes back on that connection (Apache's `KeepAliveTimeout` of 5s is tuned for exactly that), while a **relying party's back-channel** client — token exchange, introspection, a JWKS or metadata fetch — reuses a pooled connection (nginx's 75s is tuned for that). 60s is comfortably past any pooled back-channel round trip and does not let a browser that navigated away hold a slot for minutes. Nothing here streams or long-polls. **The five-minute numbers these specifications do quote are not this number**: an assertion's `NotOnOrAfter` and an `id_token`'s `exp` bound how long a *credential* may be presented, not how long a socket may be silent. |
| `MAX_CONNECTIONS` | 256 | The shared default. Each admitted connection may buffer one body of up to 64 KiB, well inside the ~1 GiB ceiling netget's HTTP family is held to; a protocol declares a smaller number only when its per-connection cost is larger. Refusal: **HTTP/1.1 `503 Service Unavailable` with `Retry-After`**, written straight onto the socket — the peer has sent no request line for hyper to answer — and logged `decision=fail_closed_connection_cap`. Fixed bytes, so nothing derived from an error can reach the wire. |

**The deadline covers the read and nothing else.** hyper owns every read once `serve_connection`
starts, and it keeps polling the connection for more input *while a request is being answered* —
so a deadline on those reads would be wrong here, not merely awkward. The idle bound is a
watchdog over `ConnectionActivity` instead, which reports a connection with work in flight as not
idle at all. The model round-trip, and an event a `manual` rule parked for a human
(`src/state/intercepts.rs`, 300s by default), are therefore outside every deadline by
construction: an answer that takes minutes can never close the connection it is an answer for.
That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse — TFTP evicted
live transfers because "idle" was measured wrongly.

**hyper's own `header_read_timeout` is not this bound.** Its 30-second default is inert unless
`http1::Builder::timer` is also set, which nothing here does: hyper downgrades a defaulted
duration to `None` when no timer is present and applies no deadline at all. That is why the
`peek` is not redundant.

`tests/server/oauth2/connection_bounds_test.rs` drives all three from the wire, with three
sockets on one server whose only rule is `*` → `manual`: a silent peer must be closed after the
first-byte bound, a peer that sends a request line and then stalls (slowloris) after the idle
bound, and a peer whose request is parked for a human must **not** be closed at all. The shared
driver and the removal-verification notes are in `tests/helpers/http_bounds.rs`.
`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound disappears.
