# Snowflake Protocol Implementation

Snowflake's client/driver protocol is **HTTPS + JSON**, not a bespoke binary wire
protocol: a driver POSTs to a handful of REST endpoints and reads a JSON envelope
back. This server answers those endpoints over hyper and lets the LLM play the
warehouse — it decides the session token minted at login and the rowset returned
for a query. **There is no storage**: no session table, no row store in Rust.

**State**: Experimental — LLM/handwritten, not real-client validated.
**Port**: 8085 by default (any high port). **Privilege**: `None`.
**Stack**: `ETH>IP>TCP>HTTP>Snowflake`.

> Not real-client validated. Real Snowflake connectors are hard to point at
> localhost, so the evidence is the **request/response envelope shapes** exercised
> with `reqwest` (see the tests), not a genuine driver on a live connection. TLS
> termination is out of scope — the server speaks plaintext HTTP; front it with a
> TLS proxy if a driver insists on HTTPS.
>
> `reqwest` is a generic HTTP client: it proves the server answers HTTP and that
> the JSON envelope has the fields a connector reads. It cannot prove a connector
> *accepts* them, so `Experimental` is the ceiling here until one does. That is the
> same rule that keeps `spark` at Experimental, and it is why this is not `Beta`
> however green the suite is.

## Authentication

**Nothing is verified.** The password reaches the model only as `has_password`
(a bool), and every later request reports `has_auth_token` — likewise a bool, true
for *any* `Authorization: Snowflake Token="..."` value, including one the server
never issued. The server compares no token against anything. Whether a session is
granted is entirely the model's decision from the login name, account and those
booleans, and whether a query is answered is its decision from `has_auth_token`.

An instruction like "log clients in" therefore admits everyone. That is the
protocol working as designed — netget is a roleplaying server, not a warehouse —
but it means **no authentication property may be inferred from a successful
login here**.

## Endpoints, events and actions

| Endpoint | Event | Model answers with |
|---|---|---|
| `POST /session/v1/login-request` | `snowflake_login` | `snowflake_login_success` or `snowflake_error` |
| `POST /queries/v1/query-request` | `snowflake_query` | `snowflake_query_response` or `snowflake_error` |
| `POST /session/logout-request` | `snowflake_session` (op=logout) | `snowflake_session_response` or `snowflake_error` |
| `POST /session/token-request` | `snowflake_session` (op=token_renew) | `snowflake_session_response` or `snowflake_error` |

(`/session/authenticator-request` is routed to `snowflake_login` as well.)

Every event `.with_actions(...)` and every declared event is actually emitted —
there are no advertised-but-unreachable events.

### Event data

- `snowflake_login`: `login_name`, `account`, `client_app_id`,
  `client_app_version`, `has_password` (from the `data` object of the login body).
- `snowflake_query`: `sql_text` (from `sqlText`), `has_auth_token` (whether an
  `Authorization: Snowflake Token="..."` header was present), `request_id`.
- `snowflake_session`: `operation` (`"logout"`/`"token_renew"`), `has_auth_token`.

### Actions (all sync, structured params — no raw bytes/base64)

- `snowflake_login_success` — `token` (required), `master_token`, `session_id`,
  `validity_seconds`. Wrapped into the driver-shaped `data.{token,masterToken,
  sessionId,validityInSeconds,sessionInfo,...}` envelope with `success:true`.
- `snowflake_query_response` — `rowtype` (array of `{name,type,...}`), `rowset`
  (array of rows), optional `query_id`. **Snowflake's JSON result format sends
  every cell as a string**, so `rowset` values are stringified; `rowtype`
  descriptors are augmented with defaults (`nullable`, `length`, …).
- `snowflake_session_response` — optional `data` object (renewed tokens for
  token-request; omit for logout).
- `snowflake_error` — `code` (string, e.g. `"390100"`), `message`. The only way
  to refuse a login/query. Client gets HTTP 200 with `success:false`.

### Request bodies are capped

`read_json_body` reads through `http_body_util::Limited` at `MAX_REQUEST_BYTES`
(4 MiB); it used to be an unbounded `collect()`, so an unauthenticated POST to the
login endpoint could grow the process without limit. An over-cap or unreadable body
becomes `Value::Null`, which every endpoint treats as "no fields present" — and
since each one fails closed when the model cannot answer from those fields, a
rejected body can only ever lose a login, never grant one.

## Fail-closed behaviour (the OAuth2/LDAP lesson)

There is **no permissive default**. When the model gives no usable answer, or the
LLM call fails, every endpoint returns a Snowflake error envelope
(`{"data":null,"code":...,"message":...,"success":false}`), never a
success-shaped empty result:

- **Login LLM outage** → refusal with code `390100` and a message prefixed
  `netget: authentication backend unavailable`. **No token is ever issued on
  failure.** This is distinguishable from a deliberate model denial: a model
  denial goes through `snowflake_error` and carries the model's own code/message;
  the outage path carries netget's and is logged at `error!`.
- **Login success action with an empty `token`** → treated as unusable → refusal.
- **Query LLM outage** → error envelope, code `000603` (or `000629` on overload).
- **Session token_renew with no answer** → refusal; a bare **logout** with no
  answer is acked (harmless).

HTTP status is always 200 — Snowflake reports application-level failures in the
JSON `success` flag, not the HTTP status.

## Failure behaviour

Every reply this server sends is HTTP `200` — Snowflake reports application failures in the
JSON `success` flag — so **the HTTP status carries no information at all**, and the
`success:false` envelope a model chose via `snowflake_error` is byte-identical in shape to the
one the server falls back to. The `decision=` token on the status stream and in `netget.log`
is the only place those come apart.

| Outcome | On the wire | Log |
|---|---|---|
| Login — model returned `snowflake_login_success` with a non-empty `token` | `success:true` + session token | INFO `decision=model_answer` |
| Login — model returned `snowflake_error` | `success:false` + the model's code/message | INFO `decision=model_reject` |
| Login — `snowflake_login_success` with an empty `token` | `success:false`, code `390100` | ERROR `decision=fail_closed_bad_action` |
| Login — no usable action | `success:false`, code `390100` | WARN `decision=model_silent` |
| Login — `call_llm` failed | `success:false`, code `390100`, `WireFailure` category text | ERROR `decision=fail_closed_llm_error` / `..._llm_overloaded` |
| Query — model returned `snowflake_query_response` | `success:true` + rowset | INFO `decision=model_answer` |
| Query — model returned `snowflake_error` | `success:false` + the model's code | INFO `decision=model_reject` |
| Query — no usable action | `success:false`, code `000603` | WARN `decision=model_silent` |
| Query — `call_llm` failed | `success:false`, `000603` (or `000629` on overload) | ERROR `decision=fail_closed_llm_error` / `..._llm_overloaded` |
| Session — model returned `snowflake_session_response` | `success:true` | INFO `decision=model_answer` |
| Session — model returned `snowflake_error` | `success:false` | INFO `decision=model_reject` |
| Session `token_renew` — no usable action | `success:false`, code `000603` | WARN `decision=model_silent` |
| **Session `logout` — no usable action** | **`success:true`** | WARN `decision=default_logout_ack` |
| Session — `call_llm` failed | `success:false`, code `000603` | ERROR `decision=fail_closed_llm_error` / `..._llm_overloaded` |
| Unrouted path | `success:false`, code `390318`, no LLM call | DEBUG `decision=unknown_endpoint` |

### `decision=default_logout_ack` — the one affirmative default, named as one

`handle_session` answers `success:true` for a **logout** the model did not answer. That is an
affirmative reply produced on a no-answer path, so tagging it `fail_closed_*` would be a lie:
`grep decision=fail_closed` is how an operator finds requests that were refused, and this one
was not. The token is deliberately its own word, at WARN, so it shows up when you look for it.

Why it is nonetheless defensible: this server keeps **no session state**, so there is nothing a
logout could fail to do and nothing the ack grants. A `token_renew` in the same position does
fail closed, because its success would hand out a credential. If a session store is ever added,
this arm becomes a real fail-open and must change with it.

`decision=unknown_endpoint` is the other invented token: a 390318 decided by the router before
any model call.

### Authentication is not checked, and the log says so too

Repeating the section above because it is the thing most likely to be misread from a
`decision=model_answer` line: `has_auth_token` is `true` for **any** `Authorization: Snowflake
Token="..."` header, including one this server never issued, and no signature is verified
anywhere. A `decision=model_answer` on a query therefore records that the *model* chose to
answer given a boolean — not that the requester was authenticated. The query log line carries
`auth=<bool>` so the value the model was shown is visible next to its decision.

## Architecture

- One hyper `service_fn` per connection (mirrors `oauth2`), routed by method+path.
- `spawn_with_llm_actions` binds via `create_reusable_tcp_listener` and propagates
  bind failure with `?` (so `server_startup` reports `Error`, not a phantom
  `Running`), then registers the accept-loop `JoinHandle` via
  `register_server_task()` so `stop_server` releases the socket.
- Responses are built by a local `json_200` helper that cannot panic (constant
  headers/status; body is our own serialized JSON). It does **not** reuse
  `http_common::build_safe_response` because it never needs model-supplied headers
  or arbitrary status codes — everything here is a fixed 200 JSON envelope.
- No lock is held across an `.await`.

## Not implemented

- **TLS** (plaintext HTTP only).
- **No session store** → the query endpoint cannot validate the token against
  issued ones; it surfaces `has_auth_token` and leaves the decision to the model.
- Chunked/async query execution (`/queries/v1/query-request` with
  `asyncExec:true` and the `/queries/.../result` poll loop), result chunking via
  `chunks`/`rowsetBase64`, `PUT`/`GET` stage file transfer, MFA, key-pair/SSO auth
  flows beyond the single login round-trip.

## Testing

`tests/server/snowflake/e2e_test.rs`, declared in `tests/server/mod.rs`. Drives
the real endpoints with `reqwest` against a running server with the LLM mocked.

```bash
./cargo-isolated.sh test --no-default-features --features snowflake \
    --test server -- snowflake:: --test-threads=100
```

## Example prompt

```
Start a Snowflake server on port 8085. Log clients in by issuing a session token,
and answer "SELECT * FROM customers" with a two-column rowset (ID fixed, NAME
text). Refuse logins for unknown accounts with snowflake_error code 390100.
```

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a connection task and an `AppState` entry forever,
pre-authentication, on a server that would happily accept a hundred more. It now declares both
halves; the constants and the reasoning live beside them in `src/server/snowflake/mod.rs`.

**Snowflake is the one of the eleven that looked bounded and was not**, which is worth keeping.
`tests/tcp_server_bounds_ratchet_test.rs` listed it on the cap baseline only — it appeared to
have a read deadline because its `TIMEOUT_TOKENS` include `_TIMEOUT`, and this file declares
`CODE_REQUEST_TIMEOUT = "000629"`, a Snowflake *error code* string. A token match is not a
mechanism; the server had no read bound at all.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 30s | HTTP is client-speaks-first, so a peer that has connected and sent no byte has begun no request. 30s sits between nginx's `client_header_timeout` default of 60s and Apache's `RequestReadTimeout header=20`. Enforced with `TcpStream::peek` before the socket reaches hyper, so the request line is still there for hyper afterwards. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 300s | A driver logs in, submits a query and fetches the result on the same pooled connection, with gaps while the application consumes a result set. A query being answered is a request in flight and held busy for the whole of the model round-trip. Four times nginx's `keepalive_timeout` default of 75s. |
| `MAX_CONNECTIONS` | 256 | Each admitted connection may hold one whole in-memory response body (a result set), so the cap turns that per-connection bound into a total one. Refusal: **HTTP/1.1 `503 Service Unavailable` with `Retry-After: 5`** — a 503 with `Retry-After` reads to a driver's HTTP layer as a retryable server condition. Nothing of netget's reaches the wire; the reason is logged under `decision=fail_closed_connection_cap`. |

**There is no NetGet Snowflake client**, so this bound has no peer of ours to strand — the
fourth exemption in `PROTOCOL_QUALITY.md`'s three-state test.

**The idle bound is a watchdog, not a read deadline, and that is not a stylistic choice.** hyper
owns every read once `serve_connection` starts and keeps polling the connection for new frames
*while a request is being answered*, so a deadline on those reads would fire in the middle of an
LLM round-trip. It watches `ConnectionActivity` instead, which reports a connection with work in
flight as not idle at all — so the model round-trip, and a `manual` rule parking an event for a
human (`src/state/intercepts.rs`, 300s by default), sit outside every deadline by construction.
That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse: TFTP evicted
live transfers because "idle" was measured wrongly.

**One residual, stated rather than hidden.** The first-byte bound is discharged the moment any
byte arrives, so a peer that sends one byte of a request line and then stops is bounded by the
*idle* deadline (300s) rather than by the 30s one. hyper's own `header_read_timeout` cannot
express the pair: it re-arms whenever hyper starts reading a head, an idle keep-alive connection
included, so setting it to 30s would close every pooled client between requests — collapsing the
two bounds onto one number, which is exactly what the pair exists to avoid.

`tests/server/snowflake/connection_bounds_test.rs` drives all three from the wire, over
`tests/helpers/http_bounds.rs`. Each was verified by **removing** the bound and watching the test
fail: with the `peek` deadline gone the silent peer still held the socket at 60s; with the idle
bound collapsed to 30s an answered connection was closed after 38s of silence; with the permit
released early the connection past the cap was served instead of refused.
