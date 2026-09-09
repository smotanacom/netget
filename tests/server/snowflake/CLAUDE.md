# Snowflake Server E2E Tests

`e2e_test.rs`, declared in `tests/server/mod.rs` (`pub mod snowflake;`).

## Strategy

Real Snowflake drivers are hard to point at localhost, so these tests **drive the
exact REST/JSON endpoints** a driver uses with `reqwest` against a running NetGet
server (LLM mocked) and assert the envelope shapes a genuine connector expects.
This is envelope-shape evidence, **not** a real-driver-on-a-live-connection test.

Cited driver expectations:
- Login success returns `data.token` / `data.masterToken` / `data.sessionId`; the
  connector sends the token back as `Authorization: Snowflake Token="..."`.
- Query success returns `data.rowtype` + `data.rowset` (every cell a string) with
  `queryResultFormat: "json"`.
- Failures are HTTP 200 with `success:false` and a `code`.

## Tests & LLM call budget (all mocked)

| Test | Flow | Mock LLM calls |
|---|---|---|
| `test_snowflake_login_and_query` | startup + login + query | 3 |
| `test_snowflake_login_refused` | startup + refused login | 2 |

**Total: 7** (under the ~10 budget). Each finishes `server.verify_mocks().await?`.

`llm_failure_test` is the third test and covers the case the other two cannot: not a
model *denial* (that is `test_snowflake_login_refused`) but netget failing to reach a
model at all. Those two must not be the same thing, and the dangerous direction is
one-way — a login endpoint that fell open on an outage would issue a session to anyone
who asked while the backend was down. It asserts `success:false` with `data:null`, and
that the `message` on the wire carries none of the backend URL, model name or retry
wording; a Snowflake driver prints that string to a human.

## What each asserts

- `test_snowflake_login_and_query`: `success:true`, `data.token` matches the
  minted token, `masterToken`/`sessionId` present; then a query with that token
  returns `rowset == [["1"]]`, `returned == 1`, `queryResultFormat == "json"`.
- `test_snowflake_login_refused`: **fail-closed shape** — `success:false`, error
  `code == "390100"`, and `data` is null (no token leaked on a refusal).

Localhost only (`127.0.0.1`), plaintext HTTP (the server does not do TLS).
