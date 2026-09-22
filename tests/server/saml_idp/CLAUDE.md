# saml_idp E2E tests

Three mocked tests over `reqwest`. Feature gate `saml-idp`; declared in `tests/server/mod.rs`
as `pub mod saml_idp;` (a test directory not declared there is silently never compiled).

**Three files, eight tests**, and this doc named none of them until 22 September 2026:

| file | tests | what it holds |
|---|---|---|
| `e2e_test.rs` | 4 | the SSO endpoints and the assertion the model authors |
| `hardening_test.rs` | 2 | **two ways the IDP could be made to answer `2xx` without a decision behind it** |
| `connection_bounds_test.rs` | 2 | the first-byte and idle deadlines and the connection cap |

`hardening_test.rs` is the one to read first. An identity provider that returns success
without a decision is the fail-open shape the project `CLAUDE.md` calls the most dangerous
pattern in this codebase — the OAuth2 case, where no action meant a hardcoded token and a
model's explicit denial was indistinguishable from silence.

```bash
./cargo-isolated.sh test --no-default-features --features saml-idp \
    --test server -- --test-threads=100 saml_idp
```

## What is asserted, and what deliberately is not

`saml_idp` holds no key and signs nothing. These tests therefore assert on the **HTTP-POST
binding** — that a well-formed base64 `SAMLResponse` decoding to the handler's exact assertion
XML is posted to the `acs_url` the handler supplied — and never on authenticity.

`test_saml_idp_sso_posts_assertion_to_acs_url` contains a positive assertion that the decoded
assertion carries **no** `ds:Signature`. That is not a style check: it is there so that adding
signing later fails this test and forces `src/server/saml_idp/CLAUDE.md` and this suite to be
updated together, instead of leaving a suite that silently reads as if signing were covered.

## Tests

| Test | Covers | LLM calls |
|---|---|---|
| `test_saml_idp_sso_posts_assertion_to_acs_url` | `/sso` → auto-submitting POST form; `acs_url` substituted; base64 round-trip; RelayState echoed; no signature | 2 |
| `test_saml_idp_escapes_relay_state` | A `RelayState` full of markup is echoed **escaped**, not raw and not dropped | 2 |
| `test_saml_idp_metadata_and_error_response` | `/metadata` served verbatim as `application/samlmetadata+xml`; `send_error_response` honours the handler's `status_code` and escapes the message | 3 |

**Total: 7 mocked LLM calls**, under the 10-call budget. All run against the mock LLM; no
Ollama is required. Every test calls `verify_mocks()`.

## Why these particular assertions

- **`acs_url`.** The generated form's action attribute used to be the literal `{{ACS_URL}}`,
  which nothing substituted and no action could supply, so every assertion the IDP produced
  was posted to a relative path named `{{ACS_URL}}` and reached no SP. The protocol's central
  operation did not work, and there was no test. `form_action()` now asserts the exact URL.
- **Base64 round-trip.** NetGet owns the encoding; the handler supplies plain XML. Decoding
  and comparing byte-for-byte catches both a double-encode and an encoder that mangles the
  payload.
- **Escaping.** `relay_state` and `error_message` are model output derived from an untrusted
  request, rendered into a page the browser auto-submits. Both tests assert the escaped form
  is *present*, not merely that the raw form is absent — otherwise dropping the value entirely
  would pass.
- **`status_code`.** It is a model-supplied parameter, and out-of-range values used to panic
  the connection task. 403 proves the value is honoured; `build_safe_response` handles the
  out-of-range case and is not re-tested here.

## Expected runtime

~3s for the suite. No real Ollama, no network beyond loopback.

## Known gaps

- No test pairs the IDP with `saml_sp` on a second port and follows the POST through. Worth
  adding when a two-server harness exists; each half is covered on its own today.
- Inbound `AuthnRequest` parsing is not exercised, because the server does not parse it: the
  binding hands the raw text to the handler.
