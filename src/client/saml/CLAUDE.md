# SAML Client Implementation

## Overview

This implements a SAML 2.0 Service Provider (SP) client that can initiate authentication with a SAML Identity Provider (
IdP). The client allows LLM-controlled Single Sign-On (SSO) operations including generating authentication requests and
validating SAML assertions.

## Library Choices

### XML Processing

- **quick-xml** (v0.37) - Fast, lightweight XML parser and writer
    - Used for generating AuthnRequest XML
    - Used for parsing SAML Response and Assertion XML
    - Provides streaming parser for efficient memory usage
    - Already used in the codebase for XML-RPC

### Encoding/Compression

- **base64** (v0.22) - Base64 encoding/decoding for SAML messages
- **flate2** (v1.0) - DEFLATE compression for HTTP-Redirect binding
- **urlencoding** (v2.1) - URL encoding for redirect parameters

### Utilities

- **uuid** (v1.11) - Generate unique request IDs
- **chrono** (v0.4) - Timestamp generation for SAML requests

## Architecture

### SAML Protocol Flow

1. **Initialization**: Client connects to IdP URL, stores configuration
2. **SSO Initiation**:
    - Generate SAML AuthnRequest with unique ID
    - Encode request (deflate + base64 for HTTP-Redirect, or base64 for HTTP-POST)
    - Build SSO URL with encoded request
    - LLM receives SSO URL to direct user
3. **Assertion Validation**:
    - Receive base64-encoded SAML Response
    - Decode and parse XML response
    - Extract status code, subject, and attributes
    - LLM processes authentication result

### HTTP Bindings Supported

1. **HTTP-Redirect** (Primary)
    - AuthnRequest is DEFLATE-compressed, base64-encoded, URL-encoded
    - Sent as query parameter: `?SAMLRequest=...&RelayState=...`
    - Lightweight, suitable for browser redirects

2. **HTTP-POST** (Planned)
    - AuthnRequest is base64-encoded (no compression)
    - Sent as form POST parameter
    - Supports larger requests

### State Management

Client stores in `protocol_data`:

- `idp_url`: Identity Provider endpoint URL (the address the client was connected to)
- `entity_id`: Service Provider entity identifier (default: `urn:netget:sp`)
- `acs_url`: Assertion Consumer Service URL (default: `http://localhost:8080/saml/acs`)
- `binding`: SAML binding type (`redirect` or `post`; default `redirect`)
- `request_id`: Generated request ID for validation
- `sso_url`: Complete SSO URL for user redirection

The first three of those come from **startup parameters**, and until recently did not:
`connect()` dropped `ctx.startup_params` on the floor, so all three declared parameters were
advertised knobs that did nothing, and `connect_with_llm_actions` seeded its defaults under a
comment claiming they "can be overridden by startup params". An operator who set an
`entity_id` got `urn:netget:sp`; one who asked for the HTTP-POST binding got a redirect.
(`startup_param_drift_test` does not catch this shape — its rule is "the name appears nowhere
else in the protocol's directory", and all three names appear here as `protocol_data` keys.)

A `binding` that is neither `redirect` nor `post` is now an `Err` at connect rather than a
silent downgrade: `build_sso_request` treats anything that is not `post` as `redirect`, so a
typo would have produced a redirect binding while the operator believed otherwise.

### Escaping

`entity_id`, `acs_url` and the generated `request_id`/timestamp go into the AuthnRequest as
XML attribute values and element text, and are escaped by `escape_xml` (`& < > " '`). A `"` in
`acs_url` previously closed the `AssertionConsumerServiceURL` attribute early; the request is
deflated and base64-encoded immediately afterwards, so a malformed one was invisible until the
IDP rejected it.

## LLM Integration

### Connection Event

Triggered when client is initialized:

```json
{
  "event": "saml_connected",
  "data": {
    "idp_url": "https://idp.example.com/saml/sso",
    "sso_url": "https://idp.example.com/saml/sso?SAMLRequest=...",
    "request_id": "_12345678-1234-1234-1234-123456789012"
  }
}
```

### Response Event

Triggered when assertion is validated:

```json
{
  "event": "saml_response_received",
  "data": {
    "success": true,
    "status_code": "urn:oasis:names:tc:SAML:2.0:status:Success",
    "assertion": {
      "subject": "user@example.com",
      "status_code": "urn:oasis:names:tc:SAML:2.0:status:Success"
    },
    "attributes": {
      "email": "user@example.com",
      "firstName": "John",
      "lastName": "Doe"
    }
  }
}
```

### Actions

#### Async Actions (User-triggered)

1. **initiate_sso** - Start authentication flow
   ```json
   {
     "type": "initiate_sso",
     "relay_state": "/protected/resource",
     "force_authn": false
   }
   ```

2. **validate_assertion** - Validate SAML response from IdP
   ```json
   {
     "type": "validate_assertion",
     "saml_response": "PHNhbWxwOlJlc3BvbnNlLi4uPg=="
   }
   ```

3. **disconnect** - Close SAML client
   ```json
   {
     "type": "disconnect"
   }
   ```

#### Sync Actions (Response to events)

1. **parse_assertion** - Parse assertion from response
   ```json
   {
     "type": "parse_assertion",
     "response_xml": "<samlp:Response...>"
   }
   ```

## Limitations

### Security Considerations

1. **No Signature Verification** - this client verifies NOTHING
    - No `<ds:Signature>` is checked, on the response or on any assertion inside it. There is
      no key here to check one against: no certificate is configured, stored or fetched.
    - Neither is the issuer, the audience restriction, `NotBefore`/`NotOnOrAfter`,
      `InResponseTo`, nor assertion-ID replay.
    - So **a forged assertion is accepted exactly as readily as a genuine one**, and the
      `success: true` this client reports to the model is a statement about XML, not about
      authentication. Say so wherever this client is described; it is the whole attack.
    - Signature verification would need `xmlsec` integration and a trust store, neither of
      which exists. Until then this is a simulator for exercising IdPs and a honeypot —
      **never an access-control boundary.**

2. **No Certificate Management** - No handling of X.509 certificates
    - Production SAML requires certificate-based trust
    - Would need certificate storage and validation

3. **Basic XML Parsing** - Reads a few fields; validates nothing
    - Extracts the top-level status code, the subject's `NameID`, and attribute values
    - Does not validate any SAML specification requirement
    - `success` means "the document's top-level `<StatusCode>` says Success", nothing more

   **What that reading has to get right, because nothing else guards it.** With no signature
   check, the only thing standing between a forged `<samlp:Response>` and a reported sign-in
   is that the status reported is the status the document actually carries. Three ways it
   was not, all fixed and pinned by `tests/client/saml/response_parsing_test.rs`:

    - `status_code.contains("Success")` was the success test. SAML nests `<StatusCode>` inside
      `<StatusCode>` to carry second-level detail, and every `StatusCode` seen overwrote the
      last, so `<StatusCode Value="…:Requester"><StatusCode Value="…:Success"/></StatusCode>`
      — a *refusal* — was reported to the model as `success: true`. It is now an equality
      check against `SAML_STATUS_SUCCESS`, on the `<StatusCode>` that is a direct child of
      `<Status>`, taking the first one only.
    - Only `Event::Empty` was matched, so the outer element of that same nesting (a `Start`)
      was skipped entirely and the inner one became "the" status. Both are matched now.
    - Element names were compared as raw qualified bytes (`b"saml:NameID"`). SAML fixes the
      namespace URIs but not the prefixes, and Shibboleth and ADFS emit `saml2:` — so a
      response from either parsed as having no subject and no attributes, silently.
      `local_name` strips the prefix.

   Also: the **first** `NameID` is the subject, so a later one — in `<Advice>`, or in a second
   assertion appended after the real one — cannot replace it; nesting past `MAX_XML_DEPTH`
   (64) is refused; and `MAX_ASSERTION_ATTRIBUTES` (128) bounds the map that is serialised
   into an LLM prompt.

   **Entity expansion cannot happen here, and it is worth knowing why rather than assuming.**
   `quick_xml` never processes a DTD: a `<!DOCTYPE>` with an internal subset arrives as an
   opaque `Event::DocType` that nothing acts on, and `unescape()` resolves only the five
   predefined entities plus numeric character references, so `&lol9;` is an unrecognised
   symbol rather than an expansion. Billion laughs is structurally impossible, not merely
   unobserved, and the test asserts it so that swapping in a DTD-honouring parser fails here
   instead of in production. Nothing in this parser recurses, so there is no stack-overflow
   path either.

4. **No Encryption** - SAML assertions are not encrypted
    - Some IdPs require encrypted assertions
    - Would need XML encryption support

### Protocol Support

1. **HTTP-Redirect Only** - HTTP-POST binding partially implemented
2. **SP-Initiated SSO Only** - IdP-initiated flow not supported
3. **No Logout** - Single Logout (SLO) not implemented
4. **No Metadata** - No support for SAML metadata exchange

### Known Issues

1. **Timestamp Validation** - Does not validate NotBefore/NotOnOrAfter conditions
2. **Audience Validation** - Does not validate audience restriction
3. **InResponseTo** - Does not validate InResponseTo matches request ID
4. **Replay Protection** - No mechanism to prevent assertion replay attacks

## Testing Strategy

### Manual Testing

1. Use a public SAML test IdP (e.g., samltest.id)
2. Configure client with IdP metadata
3. Initiate SSO and follow redirect
4. Validate response manually

### E2E Testing

- Requires running SAML IdP (e.g., SimpleSAMLphp)
- Test successful authentication flow
- Test failed authentication
- Test attribute extraction

## Example Usage

```rust
// Open SAML client
let client_id = open_client(
    "saml",
    "https://idp.example.com/saml/sso",
    "Authenticate user with SAML IdP",
    Some(json!({
        "entity_id": "https://myapp.com/saml/sp",
        "acs_url": "https://myapp.com/saml/acs"
    }))
).await?;

// Initiate SSO
execute_action(client_id, json!({
    "type": "initiate_sso",
    "relay_state": "/dashboard",
    "force_authn": false
})).await?;

// After user completes authentication and returns with SAML response:
execute_action(client_id, json!({
    "type": "validate_assertion",
    "saml_response": "PHNhbWxwOlJlc3BvbnNlLi4uPg=="
})).await?;
```

## Future Enhancements

1. **Signature Verification** - Integrate xmlsec for XML signature validation
2. **Certificate Management** - Handle X.509 certificates properly
3. **Full SAML Compliance** - Implement all required validations
4. **HTTP-POST Binding** - Complete POST binding implementation
5. **IdP-Initiated SSO** - Support unsolicited responses
6. **Single Logout** - Implement SLO protocol
7. **Metadata Support** - Generate and consume SAML metadata XML
8. **Assertion Encryption** - Support encrypted assertions

## References

- [SAML 2.0 Specification](https://docs.oasis-open.org/security/saml/v2.0/)
- [SAML 2.0 Bindings](https://docs.oasis-open.org/security/saml/v2.0/saml-bindings-2.0-os.pdf)
- [SAML 2.0 Profiles](https://docs.oasis-open.org/security/saml/v2.0/saml-profiles-2.0-os.pdf)


## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running SAML client. The handle is
registered **before** the `saml_connected` LLM call — which this client awaits inline in
`connect()` — because a dashboard-created client defaults to a `*` → manual rule and that
call can park for minutes waiting for a human.

The old "poll `get_client()` every 5 s" task is gone: the command loop is now this client's
long-lived task and ends when the client is removed or an injected `disconnect` arrives.

Adopting the channel closed a second hole: **the connected-event handler used to discard
the model's actions entirely** (it matched `Ok(_result)` and logged "SAML client ready"), so
`initiate_sso` was unreachable except by a human. Both paths now go through one
`apply_action`, which implements all three verbs — `saml_initiate_sso`,
`saml_validate_assertion` and `saml_parse_assertion` (the last had no implementation at all;
it now parses and raises `saml_response_received` through the same helper
`validate_assertion` uses).

**Outcome semantics — `Executed`, never `Sent`.** SAML puts nothing on a socket NetGet owns:
an AuthnRequest is carried by the user's browser. So the outcomes are
`Executed { detail: "saml_initiate_sso: AuthnRequest built, SSO URL <url>" }`,
`Executed { detail: "saml_validate_assertion: response parsed and reported" }` and
`Executed { detail: "saml_parse_assertion: status <status> (success=<bool>)" }`. A response
that cannot be decoded or parsed is an `Err` — never a success, and nothing on this path can
report an authenticated session it did not get. An unknown action name is
`Rejected { error }`; `disconnect` is `Disconnected`.

`saml_response_received` fires from its own registered task for an injected action
(`Dispatch::Deferred`), so a manual rule parking that LLM call cannot wedge the command
loop. `Dispatch` is `pub` because `initiate_sso` / `validate_assertion` are.
