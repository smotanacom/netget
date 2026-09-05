# saml_idp — SAML 2.0 Identity Provider simulator

Serves the IDP side of the SAML 2.0 Web Browser SSO profile over hyper HTTP/1.1 and asks the
model what to answer. `DevelopmentState::Experimental`, group `Authentication`, keywords
`saml idp` / `saml identity provider` / `identity provider` / `idp` / `saml-idp`. Feature
`saml-idp = []` — no dependencies beyond what the binary already links.

**Read this first: there is no signing key in this protocol.** The model writes the assertion
XML and NetGet only base64-encodes it into the HTTP-POST form. An assertion carries a
`<ds:Signature>` only if the model invents one, and an invented signature will not verify
against anything. Inbound `AuthnRequest` signatures are not checked either, there is no replay
protection, and no user exists. A real SP configured to require signed assertions — which is
most of them — will reject what this produces. Use it to exercise SPs that accept unsigned
assertions, and as a honeypot.

The `description()` used to read "generates signed SAML assertions". It does not, and now says
so; the same claim must not come back into `metadata()`, the docs or the action descriptions.

## Files

| File | Contents |
|---|---|
| `mod.rs` | `SamlIdpServer::spawn_with_llm_actions`, `handle_saml_idp_request`, `build_safe_response` |
| `actions.rs` | `SamlIdpProtocol` (`Protocol` + `Server`), three sync actions, `build_saml_post_form`, `escape_html`, `SAML_IDP_REQUEST_EVENT` |

No `startup_params`: `get_startup_parameters()` is the empty default, so `StartupParams` rejects
any key a caller passes. The entity ID, endpoints and attribute policy all come from the
instruction.

## One event, three actions

`saml_idp_request` fires for every request, carrying `method`, `path`, `query`, `headers`,
`body` and `client_ip`. There is no routing: `/sso`, `/SingleSignOnService`, `/metadata` and
anything else all arrive the same way and the model decides from `path`.

Bindings: HTTP-Redirect puts the `AuthnRequest` in `query`, HTTP-POST puts it in `body`. Both
reach the model as text — it is the model's job to base64-decode and read the request if the
scenario needs it.

| Action | Produces |
|---|---|
| `send_saml_response` | `200 text/html` — an auto-submitting HTTP-POST form carrying the assertion |
| `send_metadata` | `200 application/samlmetadata+xml` |
| `send_error_response` | model-chosen status (default 403), an HTML error page |

The event's `.with_actions(...)` holds all three real definitions. That list — not
`get_sync_actions()` — is what `call_llm` advertises to the model.

Executors return `ActionResult::Output` holding `{"status", "headers", "body"}`, which
`handle_saml_idp_request` merges into the response.

## `acs_url` is required, and used to be missing entirely

`send_saml_response` takes `assertion_xml`, **`acs_url`** and optional `relay_state`.

`acs_url` is new and required. The generated form's action attribute was previously the literal
string `{{ACS_URL}}` — nothing ever substituted it, and the action had no parameter that could
have. Every assertion this IDP produced was posted by the browser to a relative path named
`{{ACS_URL}}` and reached no SP at all: the protocol's central operation did not work. Pass the
`AssertionConsumerServiceURL` from the AuthnRequest, or the SP's configured ACS endpoint.

## Base64 is ours, not the model's

The model supplies **plain assertion XML**. `build_saml_post_form` base64-encodes it into the
`SAMLResponse` form field. Base64 on the wire is what SAML specifies, but the project rule
against handing models encoded blobs still applies to the action parameters — do not add a
parameter that asks for base64, and do not accept pre-encoded XML.

## HTML escaping

`escape_html` covers `& < > " '` and is applied to `acs_url`, `relay_state` and
`error_message`. All three are model output derived from an untrusted request; unescaped, a `"`
broke out of the surrounding attribute and injected markup into a page the browser renders and
auto-submits. `RelayState` is the sharp one, because it is normally echoed straight back from
whatever the SP sent.

The base64 `SAMLResponse` field is safe by construction (the alphabet excludes `"` and `<`).

## Nothing here may panic

`build_safe_response` is the only place a `Response` is built: an out-of-range status becomes
500 and a header hyper rejects is dropped with a warning.

`send_error_response` documents `status_code` as a model-supplied parameter, and the old code
did `Response::builder().status(status as u16)…body(..).unwrap()`, so `status_code: 1000`
panicked inside the connection task instead of answering. Local copy of
`http_common::handler::build_safe_response`, which the `saml-idp` feature cannot reach because
`http_common` is gated on `feature = "http"`.

## Failure handling — the peer always gets an answer, and never a diagnosis

`handle_saml_idp_request` has exactly one reply for everything it cannot turn into a SAML
Response: `fail_closed_response`. It is never a 2xx and never a `SAMLResponse` form, because a
2xx carrying an assertion is the only thing an SP accepts as a sign-in and no failure path can
produce one.

| Situation | Wire | Log |
|---|---|---|
| Model returns `send_saml_response` / `send_metadata` (status < 400) | its response | `decision=model_answer` |
| Model returns `send_error_response` (status ≥ 400) | the model's status and page | `decision=model_reject` |
| Model returns zero actions | 500 | `decision=fail_closed_no_action` |
| Actions ran but none yielded a status/headers/body | 500 | `decision=fail_closed_unusable_output` |
| LLM call errored or timed out | 503 + `Retry-After: 1` when overloaded, else 500 | `decision=fail_closed_llm_error category=overloaded\|unavailable` |

The last row's split is deliberate: `WireFailure::Overloaded` is transient, so a client should
back off and retry rather than record a permanent fault.

The body on every fail-closed path is `WireFailure::prefixed_text()`, a `&'static str`. **The
error itself never reaches the socket** — not the backend URL, the model name, a file path or
an `anyhow` chain. It goes to `tracing::error!` and the status stream. `tests/wire_failure_test.rs`
fails the build if the leaked idioms reappear; the E2E test
`test_saml_idp_fails_closed_when_model_answers_nothing` additionally asserts the live response
body carries none of those tokens.

`fail_closed_unusable_output` is the row that used to be missing: the response loop defaulted
to status 200 with an empty body, so a model whose output was not response-shaped JSON produced
`200 OK` with nothing in it — read by an SP as a completed response with no assertion rather
than as a server fault.

## Storage

None, per the project rule. No sessions, no user directory, no issued-assertion log. If a
scenario needs continuity, the model keeps it in server memory or the instruction states a
rule.

## Not implemented

XML signing and signature verification, certificate/key management, SingleLogout, artifact
binding, encrypted assertions, MFA, replay protection, and TLS.

## Examples

```text
Start a SAML Identity Provider on port 8080.
On /metadata return an EntityDescriptor with entityID http://localhost:8080 and an
HTTP-POST SSO endpoint at http://localhost:8080/sso.
On /sso authenticate everyone as 'testuser' with email test@example.com, issue an
assertion valid for one hour, and post it to http://localhost:8081/acs.
```

Deterministic equivalent — no LLM call per request:

```json
"event_handlers": [{"event_pattern": "saml_idp_request", "handler": {"type": "static",
  "actions": [{"type": "send_saml_response",
    "assertion_xml": "<saml:Assertion xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\"><saml:Subject><saml:NameID>testuser</saml:NameID></saml:Subject></saml:Assertion>",
    "acs_url": "http://localhost:8081/acs"}]}}]
```

## Tests

`tests/server/saml_idp/e2e_test.rs`, declared in `tests/server/mod.rs`. Mocked LLM plus
`reqwest` as a plain HTTP client — there is no SAML library anywhere in the suite, so the
tests assert on the *binding* (base64 `SAMLResponse` posted to the caller's `acs_url`,
`RelayState` echoed and escaped, metadata media type, model-chosen error status) and never on
authenticity. See `tests/server/saml_idp/CLAUDE.md`.

Pairs naturally with `saml_sp` on another port: point `acs_url` at the SP's `/acs`.

## References

SAML 2.0 Core, SAML 2.0 Web Browser SSO Profile, SAML 2.0 Bindings (OASIS).
