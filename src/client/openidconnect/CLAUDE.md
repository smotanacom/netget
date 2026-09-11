# OpenID Connect Client Implementation

## Overview

The OpenID Connect (OIDC) client drives OAuth2/OIDC authentication flows for NetGet: discovery,
authorization code, device code, password and client credentials, with the LLM choosing which
flow to run and what to do with the result.

**Read this first: no ID token is verified.** The `openidconnect` crate *can* check a JWT's
signature, issuer, audience and nonce — that is `id_token.claims(&client.id_token_verifier(),
&nonce)` — and this client never calls it. Tokens are taken as opaque strings and stored, so a
forged or expired `id_token` is accepted exactly as readily as a genuine one. Nothing
downstream re-checks it either. Treat this as a client for *exercising providers*, not as an
authentication boundary, and do not let "the openidconnect crate validates tokens" back into
these docs: the crate offers validation, this code declines it. `metadata().notes` says the
same.

**Tokens do not reach the model.** `oidc_token_received` reports `access_token`, `id_token` and
`refresh_token` as `"[REDACTED]"` — or `""` when absent — matching the sibling `oauth2` client,
which has always done this. The values are held in `protocol_data` and every action that needs
one (`fetch_userinfo`, `refresh_token`, `validate_token`) reads it back from there, so the
model has no use for the secret; sending it copied a live bearer token into the LLM prompt,
into `netget.log` and onto the TUI status stream. If you add an event, redact there too.

## Library Choice

**Primary**: `openidconnect` crate v3.5 + direct HTTP requests

The `openidconnect` crate is the official Rust implementation of OpenID Connect, providing:

- Full OpenID Connect Discovery (`.well-known/openid-configuration`)
- OAuth2 flows: Password and Client Credentials (via library APIs)
- Token validation and JWT parsing — **offered by the crate, not used here**; see the warning
  above
- Type-safe API with strong guarantees
- Built on top of the `oauth2` crate
- Async HTTP client support via `reqwest`

**Additional implementations**:

- **Device Code Flow (RFC 8628)**: Direct HTTP POST using `reqwest`
    - openidconnect v3.5 doesn't expose device code APIs
    - Manual implementation following RFC 8628 specification
- **Authorization Code Flow**: Direct HTTP GET/POST + local `TcpListener`
    - Requires local callback server not provided by openidconnect
    - Manual query parameter parsing and code exchange

**Why this approach**:

- Hybrid: Use openidconnect for discovery and standard flows
- Direct HTTP for flows requiring additional infrastructure
- Maintains compatibility with all OIDC providers

## Architecture

### Connection Model

The OIDC client is **logically connected** (not a persistent TCP connection):

- Connects to an OpenID Connect provider via HTTP/HTTPS
- Discovery happens automatically on connection
- Stores provider metadata and configuration
- Makes HTTP requests on-demand for tokens and user info

### State Management

Client state stored in `protocol_data`:

- `provider_url`: OIDC provider issuer URL
- `provider_metadata`: Discovered configuration (endpoints, scopes)
- `client_id`: OAuth2 client identifier
- `client_secret`: OAuth2 client secret (if confidential client)
- `access_token`: Current access token
- `id_token`: OpenID Connect ID token (JWT with user claims)
- `refresh_token`: Refresh token for obtaining new access tokens
- `redirect_uri`: Redirect URI for authorization code flow

### OAuth2/OIDC Flows Supported

1. **Resource Owner Password Credentials** (`exchange_password`)
    - User provides username and password directly
    - NetGet exchanges credentials for tokens
    - **Pros**: Simple, no browser redirect
    - **Cons**: Less secure (user trusts NetGet with credentials)
    - **Use case**: Internal apps, testing, automation

2. **Client Credentials** (`exchange_client_credentials`)
    - Machine-to-machine authentication
    - Uses client ID and client secret only
    - No user context
    - **Use case**: Service accounts, API access

3. **Device Code Flow** (`start_device_flow`) - **RFC 8628**
    - User authenticates via browser on another device
    - NetGet displays code and URL for user
    - Polls for completion with proper interval and expiration handling
    - **Use case**: CLI apps, limited input devices
    - **Implementation**: Direct HTTP POST to device authorization endpoint, background polling task

4. **Authorization Code Flow** (`start_authorization_code_flow`)
    - Traditional web flow with browser redirect
    - Local HTTP server receives callback on configurable port
    - CSRF protection via state parameter
    - **Use case**: Web apps, desktop apps with browser
    - **Implementation**: Local TcpListener on localhost, query parameter parsing, code exchange

### LLM Integration

#### Events Triggered

1. **`oidc_discovered`**: Provider configuration discovered
    - Issuer URL, endpoints, supported scopes
    - LLM can decide which flow to use

2. **`oidc_token_received`**: Tokens received
    - Access token, ID token, refresh token
    - LLM can fetch user info or make API calls

3. **`oidc_userinfo_received`**: User information retrieved
    - Subject (user ID), claims (name, email, etc.)
    - LLM can process user data

#### Actions Available

**Async (User-Triggered)**:

- `discover_configuration`: Discover provider metadata
- `start_device_flow`: Begin device code authentication (RFC 8628)
- `start_authorization_code_flow`: Begin authorization code flow with local callback server
- `exchange_password`: Authenticate with username/password
- `exchange_client_credentials`: Get service account token
- `refresh_token`: Refresh access token
- `fetch_userinfo`: Get user information
- `disconnect`: Close connection

**Sync (Response Actions)**:

- `fetch_userinfo`: Automatically fetch user info after tokens
- `refresh_token`: Refresh expired tokens

## Implementation Details

### Discovery Process

```rust
// Automatic discovery on connection
let issuer_url = IssuerUrl::new("https://accounts.google.com")?;
let provider_metadata = CoreProviderMetadata::discover_async(
    issuer_url,
    async_http_client,
).await?;

// Discovered endpoints stored:
// - authorization_endpoint
// - token_endpoint
// - userinfo_endpoint
// - supported_scopes
```

### Token Exchange (Password Flow)

```rust
let client = CoreClient::from_provider_metadata(
    provider_metadata,
    OidcClientId::new(client_id),
    Some(ClientSecret::new(client_secret)),
);

let token_response = client
    .exchange_password(
        &ResourceOwnerUsername::new(username),
        &ResourceOwnerPassword::new(password),
    )
    .add_scope(Scope::new("openid".to_string()))
    .add_scope(Scope::new("profile".to_string()))
    .request_async(async_http_client)
    .await?;

// Extract tokens
let access_token = token_response.access_token().secret();
let id_token = token_response.extra_fields().id_token();
let refresh_token = token_response.refresh_token();
```

### UserInfo Retrieval

```rust
let userinfo: CoreUserInfoClaims = client
    .user_info(AccessToken::new(access_token), None)?
    .request_async(async_http_client)
    .await?;

// Access user claims
let subject = userinfo.subject(); // User ID
let email = userinfo.email();     // Optional
let name = userinfo.name();       // Optional
```

### Device Code Flow Implementation

```rust
// 1. POST to /device/code endpoint
let device_auth_url = format!("{}/device/code", issuer_url);
let response = http_client.post(&device_auth_url)
    .form(&[
        ("client_id", client_id),
        ("scope", scopes),
    ])
    .send()
    .await?;

// 2. Parse device_code, user_code, verification_uri
let device_code = response["device_code"].as_str()?;
let user_code = response["user_code"].as_str()?;
let verification_uri = response["verification_uri"].as_str()?;

// 3. Display to user
info!("Go to {} and enter code: {}", verification_uri, user_code);

// 4. Poll token endpoint in background
tokio::spawn(async move {
    loop {
        let token_response = http_client.post(&token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", client_id),
            ])
            .send()
            .await?;

        // Handle authorization_pending, slow_down, expired_token
        match token_response.status() {
            200 => break, // Success
            400 => check_error_and_retry(),
        }

        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
});
```

### Authorization Code Flow Implementation

```rust
// 1. Generate CSRF state parameter
let state: String = rand::thread_rng()
    .sample_iter(&rand::distributions::Alphanumeric)
    .take(32).map(char::from).collect();

// 2. Build authorization URL
let auth_url = format!(
    "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
    auth_endpoint,
    urlencoding::encode(&client_id),
    urlencoding::encode(&redirect_uri),
    urlencoding::encode(scopes),
    urlencoding::encode(&state)
);

info!("Open this URL in your browser:\n{}", auth_url);

// 3. Start local callback server
let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;

tokio::spawn(async move {
    let (mut socket, _) = listener.accept().await?;

    // 4. Parse HTTP request and extract query parameters
    let query_string = extract_query_string(&request)?;
    let params = parse_query_params(&query_string)?;

    // 5. Verify state parameter
    if params["state"] != expected_state {
        return Err("CSRF protection failed");
    }

    // 6. Exchange authorization code for tokens
    let token_response = http_client.post(&token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &params["code"]),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
        ])
        .send()
        .await?;

    // 7. Store tokens
    store_tokens(token_response).await?;
});
```

## Limitations

1. **Device Code Flow**: Requires provider support
    - Not all OIDC providers support RFC 8628 device code flow
    - Provider must expose `/device/code` endpoint
    - Implementation uses direct HTTP requests (not openidconnect crate APIs)

2. **Authorization Code Flow**: Localhost only
    - Local HTTP server binds to 127.0.0.1 (localhost)
    - User must open browser on same machine
    - Configurable port (default 8080)
    - Implementation uses direct HTTP requests and TcpListener

3. **Token Expiration**: Manual refresh required
    - No automatic token refresh on expiry
    - LLM must explicitly call `refresh_token` action
    - Future enhancement: Automatic refresh before expiry

4. **PKCE Support**: Not explicitly configured
    - PKCE (Proof Key for Code Exchange) enhances security
    - `openidconnect` crate supports it but not enabled

5. **Certificate Validation**: Uses system defaults
    - Custom CA certificates not configurable
    - Self-signed certificates will fail validation

6. **ID Token Validation**: Basic validation only
    - JWT signature validation handled by library
    - Additional claims validation (audience, expiry) could be enhanced

## Security Considerations

1. **Credential Storage**: Tokens stored in memory only
    - Not persisted to disk
    - Lost when client disconnected
    - Refresh tokens allow re-authentication without credentials

2. **Client Secret**: Stored in plaintext in memory
    - Should only be used for confidential clients
    - Public clients (mobile/SPA) should not use client secret

3. **Password Flow**: Less secure than other flows
    - User credentials visible to NetGet
    - Only use with trusted providers
    - Prefer device code or authorization code flows when possible

4. **TLS Enforcement**: All OIDC communications over HTTPS
    - HTTP provider URLs will be rejected by most providers
    - `openidconnect` crate enforces HTTPS for security

## Testing Strategy

### Unit Tests

- Provider metadata parsing
- Token response parsing
- Action execution logic

### E2E Tests

- Use public OIDC test providers (Auth0, Keycloak)
- Test password flow with test credentials
- Test client credentials flow
- Verify token refresh
- Verify UserInfo retrieval

### Test Providers

- **Local Keycloak**: Full OIDC implementation for testing
- **Auth0**: Public test instances
- **Google**: Real provider (requires test account)

## Example Prompts

### Password Flow

```
Connect to https://login.microsoftonline.com/common/v2.0 as OpenID Connect client
Exchange username user@example.com and password mypass123 for tokens
Fetch user information
```

### Client Credentials

```
Connect to https://auth.example.com as OpenID Connect client
Use client credentials to get service account token
```

### Device Code Flow

```
Connect to https://accounts.google.com as OpenID Connect client
Start device code flow for user authentication
```

### Authorization Code Flow

```
Connect to https://auth.example.com as OpenID Connect client
Start authorization code flow with callback on port 8080
```

## Future Enhancements

1. **Automatic Token Refresh**: Background task to refresh before expiry
2. **PKCE**: Enable for authorization code flow security (Proof Key for Code Exchange)
3. **Multiple Providers**: Support connecting to multiple providers simultaneously
4. **Token Introspection**: Validate tokens via introspection endpoint
5. **Revocation**: Revoke tokens on disconnect
6. **Session Management**: Track multiple sessions with different users
7. **Browser Auto-Launch**: Automatically open browser for device code and authorization flows
8. **Custom Callback Paths**: Support configurable callback paths for authorization code flow

## References

- OpenID Connect Specification: https://openid.net/specs/openid-connect-core-1_0.html
- OAuth2 RFC 6749: https://tools.ietf.org/html/rfc6749
- Device Code Flow RFC 8628: https://tools.ietf.org/html/rfc8628
- `openidconnect` crate docs: https://docs.rs/openidconnect/
- OAuth2 flows explained: https://oauth.net/2/grant-types/


## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running OIDC client. The handle is
registered **before** discovery and the `oidc_discovered` LLM call — both awaited inline in
`connect()` — because a dashboard-created client defaults to a `*` → manual rule and that
call can park for minutes waiting for a human.

The old "poll `get_client()` every 5 s" task is gone: the command loop is now this client's
long-lived task and ends when the client is removed or an injected `disconnect` arrives.
`execute_llm_action` is now a thin wrapper over `apply_action`, which the command loop calls
too, so every flow is driven by one implementation whoever asked for it.

`apply_action` also implements `oidc_discover`, which was **advertised but unexecutable**:
`discover_configuration` is a declared action whose `Custom` result no arm handled, so it
always came back "Unknown OIDC action".

**Outcome semantics — `Executed`, never `Sent`, and the detail is derived, not assumed.**
The `openidconnect` crate owns the HTTPS socket and reports no byte count. The command loop
**awaits** the flow and then reads `access_token` back out of the client's state
(`token_state_detail`), so:

- `Executed { detail: "oidc_client_credentials: completed, an access token is stored" }`
- `Executed { detail: "oidc_password_flow: completed but no access token was stored - the
  provider did not issue one (see netget.log)" }`
- `Executed` with a specific description for `oidc_discover`, `oidc_device_flow`,
  `oidc_authorization_code` and `oidc_fetch_userinfo`
- `Rejected { error }` for an unknown action name, `Err` for a flow that could not run
  (discovery failed, no refresh token, …), `Disconnected` for `disconnect`.

**No path here may report a success for a flow that produced no token, and none may
fabricate one** — the same fail-closed discipline the OAuth2 server's fail-open bug taught.

One deliberate asymmetry: the **model's** `disconnect` still calls `remove_client` (as it
always has), while an **injected** `disconnect` only marks the client `Disconnected` and
ends the command loop. Removing the client from inside its own command loop would abort the
very task that owes the dashboard a reply.

`oidc_token_received` / `oidc_userinfo_received` fire from their own registered task for an
injected action (`Dispatch::Deferred`); the device-code and authorization-code flows keep
raising theirs inline, since they already run in their own spawned tasks.

### Startup params reach `protocol_data` now

Every flow reads `client_id` / `client_secret` out of `protocol_data`, but the generic
creation path — the dashboard form, MCP, `open_client` — only stores the validated startup
params on the client and leaves `protocol_data` `Null`, so a client created that way failed
with "Missing client configuration" on the LLM path just as much as the injected one.
`connect` now seeds `protocol_data` from `startup_params` once, without overwriting anything
already set.

### The `flow` startup parameter starts a flow

`flow` names which OAuth2/OIDC flow to run and was read by nothing, so the flow was whichever
action the model happened to choose — an operator asking for `client_credentials` could get a
device-code prompt instead. `connect` now maps it to the matching action and runs it through
`execute_llm_action`, the same entry point the model's own flow actions use, from a
registered task (the device flow's polling and the authorization-code flow's callback server
both outlive `connect()`). The `scopes` startup parameter rides along on that action.

| `flow` | action started |
|---|---|
| `device_code` | `start_device_flow` |
| `authorization_code` | `start_authorization_code_flow` |
| `client_credentials` | `exchange_client_credentials` |
| `password` | **refused at connect** |
| anything else | **refused at connect** |

`password` is refused rather than ignored: the resource-owner password flow needs a username
and a password, which are parameters of the `exchange_password` *action* and are deliberately
not startup parameters. No value of `flow` alone could start it, so the client says so
instead of coming up and doing nothing.
