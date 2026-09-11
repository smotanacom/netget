# saml_sp — SAML 2.0 Service Provider simulator

Serves the SP side of the SAML 2.0 Web Browser SSO profile over hyper HTTP/1.1 and asks the
model what to do with each request. `DevelopmentState::Experimental`, group `Authentication`,
keywords `saml sp` / `saml service provider` / `service provider` / `sp` / `saml-sp`. Feature
`saml-sp = ["urlencoding"]`.

**Read this first: NetGet verifies nothing.** The `SAMLResponse` body is handed to the model as
text and the model decides who the user is. No XML signature is checked — there is no key here
to check one against — and neither are the issuer, the audience restriction, `NotBefore` /
`NotOnOrAfter`, or assertion-ID replay. A forged or expired assertion is accepted exactly as
readily as a genuine one. The session cookie is the bare user id, with no server-side session
store.

The `description()` used to read "validates SAML assertions" and `llm_control` listed
"assertion validation". Both now say what actually happens; do not let the claim back in. If
you want validation behaviour, it has to be spelled out in the instruction — "reject anything
whose `<saml:Issuer>` is not `https://myidp.example.com`" — and even then the model is reading
XML, not verifying cryptography.

## Files

| File | Contents |
|---|---|
| `mod.rs` | `SamlSpServer::spawn_with_llm_actions`, `handle_saml_sp_request`, `build_safe_response` |
| `actions.rs` | `SamlSpProtocol` (`Protocol` + `Server`), four sync actions, `build_authn_post_form`, `build_authn_redirect`, `escape_html`, `cookie_value`, `SAML_SP_REQUEST_EVENT` |

No `startup_params`: `get_startup_parameters()` is the empty default, so `StartupParams` rejects
any key a caller passes.

## One event, four actions

`saml_sp_request` fires for every request, carrying `method`, `path`, `query`, `headers`,
`body` and `client_ip`. There is no routing — `/login`, `/acs`, `/AssertionConsumerService`,
`/metadata` and anything else all arrive the same way and the model decides from `path`.

| Action | Produces |
|---|---|
| `send_authn_request` | `200 text/html` — redirect page or auto-submitting form carrying the AuthnRequest to the IDP |
| `process_assertion` | `200 text/html` welcome page + `Set-Cookie: session_id=…` |
| `send_metadata` | `200 application/samlmetadata+xml` |
| `send_error_response` | model-chosen status (default 403), an HTML error page |

The event's `.with_actions(...)` holds all four real definitions. That list — not
`get_sync_actions()` — is what `call_llm` advertises to the model.

`send_authn_request` takes `binding`: `HTTP-POST` builds an auto-submitting form,
anything else (default `HTTP-Redirect`) builds a meta-refresh page whose URL carries
`SAMLRequest` and `RelayState` as percent-encoded query parameters.

## Base64 is ours, not the model's

The model supplies **plain AuthnRequest XML**; `build_authn_post_form` /
`build_authn_redirect` base64-encode it. Inbound, the raw `SAMLResponse=…` body is passed
through as text and the model decodes it. Do not add an action parameter that asks the model
for base64.

## HTML escaping and the session cookie

`escape_html` covers `& < > " '` and is applied to `user_id`, the rendered `attributes`, the
`error_message`, `idp_sso_url`, `relay_state` and the assembled redirect URL. Every one of
those is model output derived from an untrusted request — most directly `user_id`, which the
model lifts out of an assertion an attacker supplied. Unescaped, a `"` broke out of the
surrounding attribute and injected markup into pages the browser renders (and, for the POST
binding, auto-submits).

`cookie_value` percent-encodes the user id before it goes into `Set-Cookie`. Raw, a `;` let a
crafted user id append cookie attributes and CR/LF let it split the response header.

## Nothing here may panic

`build_safe_response` is the only place a `Response` is built: an out-of-range status becomes
500 and a header hyper rejects is dropped with a warning.

`send_error_response` documents `status_code` as a model-supplied parameter and
`process_assertion` puts a model-supplied user id into `Set-Cookie`; the old code did
`Response::builder().status(status as u16)…body(..).unwrap()`, so `status_code: 1000` — or a
header value containing CR/LF — panicked inside the connection task instead of answering.
Local copy of `http_common::handler::build_safe_response`, which the `saml-sp` feature cannot
reach because `http_common` is gated on `feature = "http"`.

## Hostile input

Two bounds, both covered by `tests/server/saml_sp/hardening_test.rs`. For an SP the
"affirmative default" class is the vulnerability rather than a cosmetic problem: a `2xx` is the
only thing a browser reads as a completed sign-in.

- **`MAX_REQUEST_BYTES` = 256 KiB.** `/acs` takes an anonymous POST and the body reaches the
  model verbatim as prompt text, so the previous unbounded `req.collect()` let one request grow
  the process without limit and drive an LLM call with megabytes of attacker-chosen prompt.
  Over the limit is `413` and a refusal — never a truncated body, which would arrive at the
  model as a well-formed request whose assertion happened to end early.
- **`status_or` narrows a model-supplied status with `u16::try_from`.** `65736 as u16` is
  `200`. `send_error_response` — the model's only way to refuse an assertion — therefore
  arrived as the status that admits the user. Both `mod.rs` and the executor now refuse the
  wrap; the executor additionally pins `send_error_response` to 400–599.

**No XML is parsed here.** The `SAMLResponse` is passed to the model as text and NetGet never
builds a tree, so the entity-expansion (billion-laughs) and unbounded-nesting classes do not
arise on this path — the absence of a parser is, on this one axis, the safe choice. It is also
why no signature can be checked: see the warning at the top of this file. (A non-UTF-8 body is
reported to the model as `<N bytes of non-UTF-8 data…>`; it used to be base64-encoded into the
event, which the project rule forbids and which no model can decode.)

## Storage

None, per the project rule. There is no session table: `process_assertion` sets a cookie and
nothing on the server remembers it, so a later request carrying that cookie is just another
request the model must judge. If a scenario needs session continuity, the model keeps it in
server memory or the instruction states a rule.

## Not implemented

XML signature verification, certificate/trust management, SingleLogout, artifact binding,
encrypted assertions, replay protection, persistent sessions, and TLS.

## Examples

```text
Start a SAML Service Provider on port 8081.
On /login send an AuthnRequest to the IDP at http://localhost:8080/sso via HTTP-Redirect.
On /acs read the SAMLResponse, and only if its issuer is http://localhost:8080 and the
assertion has not expired, start a session for the NameID; otherwise reject with 403.
On /metadata return an EntityDescriptor with ACS at http://localhost:8081/acs.
```

Deterministic equivalent — no LLM call per request:

```json
"event_handlers": [{"event_pattern": "saml_sp_request", "handler": {"type": "static",
  "actions": [{"type": "process_assertion", "user_id": "testuser",
    "attributes": {"email": "test@example.com", "role": "user"}}]}}]
```

Note what that static handler means: it accepts every assertion unconditionally. That is fine
for testing an IDP and is exactly what a honeypot wants; it is not authentication.

## Failing closed

Three outcomes on the request path are deliberately distinct, and each is tagged in the log so
an operator can tell them apart with a grep for `decision=`:

| Situation | `decision=` | Wire |
|---|---|---|
| The model answered, choosing a 4xx (`send_error_response`) | `model_reject` | the model's status and page |
| The model answered normally | `model_answer` | the model's status and page |
| The model produced no usable action output | `fail_closed_no_answer` | `500`, `WireFailure::Unavailable` category text |
| The LLM call returned `Err` | `fail_closed_llm_error` | `503` + `Retry-After: 5` when overloaded, else `500`; category text |

Two properties hold on both fail-closed rows and must keep holding: **never a 2xx** (a 2xx is
the only thing a browser reads as a completed sign-in) and **never a `Set-Cookie`**.

The status used to default to `200`, so a model that answered with only a common/memory action
— or whose protocol result was an `ActionResult::Multiple` wrapping the real `Output`, or whose
output bytes were not JSON — produced an empty `200 OK`. There is no default any more: a
response is emitted only if a usable `Output` was actually parsed, and `Multiple` is flattened
so a wrapped `Output` is not dropped.

The body on both fail-closed rows is `WireFailure::prefixed_text()`, a `&'static str`. **Never
interpolate the error** — it names the backend URL, the model and netget's own retry machinery,
and the peer here is an untrusted browser. The full error goes to `tracing` and the status
stream. `tests/wire_failure_test.rs` fails the build if the leaked idioms reappear.

## Tests

`tests/server/saml_sp/` exists and is declared in `tests/server/mod.rs`; see
`tests/server/saml_sp/CLAUDE.md` for the strategy, the call budget and the known gaps.

```bash
./cargo-isolated.sh test --no-default-features --features saml-sp --test server -- \
    --test-threads=100 saml_sp
```

Pairs naturally with `saml_idp` on another port: point the IDP's `acs_url` at this server's
`/acs`.

## References

SAML 2.0 Core, SAML 2.0 Web Browser SSO Profile, SAML 2.0 Bindings (OASIS).
