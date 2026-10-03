# OAuth2 Client Implementation

## Overview

OAuth2 client implementation for NetGet that supports multiple OAuth2 authentication flows with LLM control. This client
enables secure authentication with OAuth2 providers using industry-standard flows.

## Library Choices

**Primary Library**: `oauth2` v4.4 - Comprehensive OAuth2 client library

- Supports all major OAuth2 flows (password, device code, client credentials, authorization code)
- Built-in PKCE support for security
- Async HTTP client integration with reqwest
- Type-safe token handling
- Refresh token support

## Architecture

### Connection Model

The OAuth2 client is **HTTP-based** and uses reqwest under the hood via the `oauth2` crate. Unlike traditional TCP-based
clients, there is no persistent connection - instead, the client makes HTTP requests to token endpoints as needed.

### OAuth2 Flows Supported

1. **Resource Owner Password Credentials Flow**
    - Direct username/password authentication
    - Simple, no browser redirect required
    - Less secure (user trusts NetGet with credentials)
    - Action: `exchange_password`

2. **Device Code Flow**
    - User authenticates via browser on another device
    - Ideal for CLI applications
    - Displays verification URL and user code
    - Polls for completion automatically
    - Action: `start_device_code_flow`

3. **Client Credentials Flow**
    - Service-to-service authentication
    - No user context
    - Simplest for machine-to-machine scenarios
    - Action: `exchange_client_credentials`

4. **Authorization Code Flow**
    - Traditional web flow with redirect
    - Most secure (user never shares password)
    - Requires callback server or manual code paste
    - PKCE protection included
    - Actions: `generate_auth_url`, `exchange_code`

### State Management

OAuth2 client stores the following in protocol_data:

- `client_id` - OAuth2 client identifier
- `client_secret` - OAuth2 client secret (optional)
- `auth_url` - Authorization endpoint URL (optional, for auth code flow)
- `token_url` - Token endpoint URL (required)
- `device_auth_url` - Device authorization URL (optional, for device code flow)
- `access_token` - Current access token (after successful auth)
- `refresh_token` - Refresh token for token renewal
- `token_type` - Token type (usually "Bearer")
- `expires_in` - Token expiration time in seconds
- `scopes` - Granted scopes
- `pkce_verifier` - PKCE verifier for auth code flow
- `csrf_token` - CSRF protection token
- `device_code` - Device code for polling
- `polling_interval` - Device code polling interval

### LLM Integration

**Event Flow**:

1. Client initialized → `oauth2_connected` event
2. LLM chooses authentication flow based on instruction
3. Token obtained → `oauth2_token_obtained` event (tokens redacted for security)
4. Device code flow → `oauth2_device_code_started` event (displays URL and code)
5. Errors → `oauth2_error` event

**Action Execution**:

- Async actions: User-triggered (e.g., "authenticate with password")
- Sync actions: Response-triggered (e.g., refresh token on expiration)
- Custom action results for each flow type

**State Machine**: Idle → Processing → Completed (no data accumulation, request-response pattern)

## Implementation Details

### Token Security

Access tokens and refresh tokens are stored in protocol_data but **redacted in LLM events** for security. The LLM sees
`"[REDACTED]"` instead of actual token values.

### Device Code Flow Polling

When device code flow is initiated:

1. Client displays verification URL and user code
2. Spawns background task to poll every N seconds (server-specified interval)
3. Polls up to 60 times (5 minutes with 5-second interval)
4. Uses direct HTTP POST to token endpoint (workaround for oauth2 crate limitation)
5. Automatically fires `oauth2_token_obtained` event on success
6. Stops polling if token obtained or timeout reached
7. Ignores `authorization_pending` and `slow_down` errors during polling

### PKCE (Proof Key for Code Exchange)

Authorization code flow automatically uses PKCE (SHA-256) for enhanced security:

1. Generates random code verifier
2. Creates SHA-256 code challenge
3. Stores verifier in protocol_data
4. Sends challenge with auth URL
5. Uses verifier when exchanging code for token

### Token Refresh

When a refresh token is available, the LLM can trigger token refresh:

```json
{
  "type": "refresh_token"
}
```

The client automatically handles refresh and fires a new `oauth2_token_obtained` event.

## Startup Parameters

Required:

- `client_id` - OAuth2 client ID from provider
- `token_url` - Token endpoint URL

Optional:

- `client_secret` - Client secret (required for some flows)
- `auth_url` - Authorization endpoint (required for auth code flow)
- `scopes` - Default scopes to request
- `device_auth_url` - Device authorization endpoint (defaults to `{remote_addr}/device/code`)

## Example Usage

### Password Flow

```
open_client oauth2 https://provider.com/oauth --client_id=my-client --client_secret=secret --token_url=https://provider.com/oauth/token "Authenticate with username 'user@example.com' and password 'secret123'"
```

### Device Code Flow

```
open_client oauth2 https://provider.com/oauth --client_id=my-client --token_url=https://provider.com/oauth/token --device_auth_url=https://provider.com/oauth/device/code "Use device code flow for authentication"
```

### Client Credentials Flow

```
open_client oauth2 https://provider.com/oauth --client_id=my-client --client_secret=secret --token_url=https://provider.com/oauth/token "Get service account token"
```

### Authorization Code Flow

```
open_client oauth2 https://provider.com/oauth --client_id=my-client --auth_url=https://provider.com/oauth/authorize --token_url=https://provider.com/oauth/token "Generate authorization URL for user authentication"
```

## Limitations

1. **No Token Revocation**: Currently does not support token revocation endpoints
2. **No Introspection**: Does not support OAuth2 introspection (RFC 7662)
3. **Callback Server**: Authorization code flow requires manual code paste (no embedded callback server)
4. **Single Provider**: One client instance per provider (cannot share tokens across multiple clients)
5. **No Dynamic Registration**: Does not support OAuth2 Dynamic Client Registration (RFC 7591)
6. **No JWT Validation**: Does not validate JWT tokens (use OpenID Connect for ID token validation)

## Protocol Information

- **Stack**: ETH>IP>TCP>HTTP>OAuth2
- **RFCs**:
    - RFC 6749 (OAuth 2.0 Authorization Framework)
    - RFC 7636 (PKCE)
    - RFC 8628 (Device Authorization Grant)
- **Port**: N/A (HTTP-based, uses provider's endpoints)
- **Security**: Supports PKCE, redacts tokens in logs and LLM events

## Dependencies

- `oauth2` crate v4.4: OAuth2 client implementation (password, client credentials, authorization code flows)
- `reqwest`: HTTP client (used directly for device code polling, also via oauth2 crate)
- Built-in types for type-safe token handling

## Testing Considerations

E2E testing requires:

1. Mock OAuth2 server or public test provider
2. Test accounts with known credentials
3. Validation of token exchange flows
4. Refresh token testing

See `tests/client/oauth2/CLAUDE.md` for test strategy.


## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running OAuth2 client. The handle is
registered **before** the `oauth2_connected` LLM call — which this client awaits inline in
`connect()` — because a dashboard-created client defaults to a `*` → manual rule and that
call can park for minutes waiting for a human.

The old "poll `get_client()` every 5 s" task is gone (it was never registered either, so
`stop_client` could not abort it). The command loop is now this client's long-lived task and
ends when the client is removed or an injected `disconnect` arrives. `execute_llm_action`
became `apply_action`, which both the connected-event handler and the command loop call, so
every flow is driven by one implementation whoever asked for it.

**Outcome semantics — `Executed`, never `Sent`, and the detail is derived, not assumed.**
The `oauth2` crate owns the HTTPS socket and reports no byte count, so `Sent { bytes_sent }`
would be invented. The command loop **awaits** the flow and then *looks at what it left
behind*:

- `Executed { detail: "oauth2_exchange_client_credentials: completed, an access token is stored" }`
- `Executed { detail: "oauth2_refresh_token: completed but no access token was stored - the
  provider did not issue one (see the oauth2_error event and netget.log)" }`
- `Executed { detail: "oauth2_generate_auth_url: authorization URL built (nothing sent; the
  user's browser carries it)" }`
- `Rejected { error }` for an unknown action name or bad parameters, `Err` for a flow that
  could not run (`exchange_code` with no stored PKCE verifier, say), `Disconnected` for
  `disconnect`.

That derivation is the point, and it is the client-side counterpart of the OAuth2 *server*
fail-open bug this repo treats as its reference case: **no path here may report a success
for a flow that produced no token, and none may fabricate a code or a token.** The
`token_state_detail` helper reads `access_token` back out of the client's state after the
flow; a refusal by the provider therefore reads as a refusal.

`oauth2_token_obtained` / `oauth2_error` still fire, but for an injected action from their
own registered task (`Dispatch::Deferred`) rather than inline — otherwise a manual rule
parking that LLM call would wedge the command loop and `send_to_client` would time out on a
flow that in fact completed. The device-code polling task keeps raising its events inline;
it is already off any command loop's critical path.

### Startup params reach `protocol_data` now

Every flow reads its configuration (`client_id`, `token_url`, `client_secret`, …) out of
`protocol_data`, but the generic creation path — the dashboard form, MCP, `open_client` —
only stores the validated startup params on the client and leaves `protocol_data` `Null`.
A client created that way arrived here unconfigured and failed with "Missing OAuth2
client_id startup parameter", on the LLM path just as much as the injected one. `connect`
now seeds `protocol_data` from `startup_params` once, without overwriting anything already
set.

## Browser build

This client is in the browser build (`crates/netget-web`). The `oauth2` crate is handed its HTTP function per request (`request_async(http_hook)`): natively that is the crate's own reqwest `async_http_client`, unchanged; on wasm32 `http_hook` sends the crate's `HttpRequest` through `crate::client::http_fetch` (hyper's HTTP/1.1 client over the page's virtual loopback, 30 s, 8 MiB, no redirects — the crate's own client follows none either) and hands back its `HttpResponse`. The device-code poll this client writes itself goes through `FetchClient` the same way. The crate enforces no scheme, so an `http://` token endpoint on the page's network works. `https://` token endpoint is refused
in the browser at connect with the reason (`http_fetch::check_url`): the transport has no TLS.
`web/test/smoke.mjs` proves it in the bundle: the client is started through `ClientForm` (it needs `client_id` and `token_url`, which `[ + OAuth2 client ]` cannot know — the dashboard's form asks for them), the model asks for a client-credentials token, NetGet's `oauth2` server's model issues one (its request arrives as `grant_type` `client_credentials`), and the token event reaches the model with the expiry the server chose; `[ send ]` repeats the exchange; an `https://` token URL is refused.


Dashboard pairing fills `/authorize` and `/token` URLs from the running local server and opens the normal form for `client_id` and any optional `client_secret`; it does not invent credentials. The browser page API opens the same focused form.
