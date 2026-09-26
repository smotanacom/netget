# PostgreSQL Client E2E Tests

Three files, declared in `tests/client/postgresql/mod.rs`. Nothing is `#[ignore]`d.

| File | Peer | Tests | LLM calls |
|---|---|---|---|
| `real_server_test.rs` | **real PostgreSQL** (`initdb` + `postgres`) + `psql` | 1 | 6 |
| `e2e_test.rs` | NetGet's own PostgreSQL server | 1 | 13 |
| `command_channel_test.rs` | NetGet's own PostgreSQL server | 1 | 0 |

## Running

`--test` names a **target**, not a module path:

```bash
./cargo-isolated.sh test --no-default-features --features postgresql \
    --test client -- client::postgresql --test-threads=100
```

## `real_server_test.rs` — the evidence the rating rests on

The peer is the PostgreSQL server itself, set up per test by `tests/helpers/real_server.rs`:
`initdb -A trust -U netget -E UTF8 --locale=C --no-sync` into the guard's temp dir (a setup
command), then `postgres -p <probed port> -k <dir> -c listen_addresses=127.0.0.1 -c fsync=off`,
ready when it logs `database system is ready to accept connections`. State is read back with
`psql` (libpq). **It fails, never skips,** when `initdb`, `postgres` or `psql` is missing,
naming `brew install postgresql@17` / `apt-get install postgresql postgresql-client`. On
Ubuntu the server binaries live in `/usr/lib/postgresql/<major>/bin`, off `PATH`; the helper
searches there, newest major first.

The server is stopped with **`SIGINT`** (fast shutdown) before the process group is killed
(`graceful_stop`). Every postmaster holds a System V shared memory segment that only a clean
shutdown removes, and macOS allows 32 system-wide: SIGKILLing a postmaster per test leaks one
segment per run (checked — `ipcs -m` shows it) until no postmaster can start.

### `postgresql_client_creates_inserts_and_selects_against_postgres` (6 calls)

`postgresql_connected` (matched on `user` `netget`) → `CREATE TABLE netget_notes`; its result
(matched on the query and `row_count` 0) → `INSERT … 'hello from the model'`; that result →
`SELECT id, body`; the SELECT's result (matched on the query, `row_count` 1 **and** `rows`
containing the inserted body) → an `INSERT` of `the model saw: <body> (id <id>)` built from
the row; that result → nothing. The last rule is listed first because its query also names
`netget_notes`. Then `psql` must print exactly the two bodies in order, and
`information_schema` must show the model's column type.

### Why this is condition 4 of the client bar

The assertions are on the server's state, written by statements the model chose — one of them
built from rows it was shown. Verified by mutation: dropping the actions the model returns for
a `postgresql_query_result` makes the test fail (the `INSERT` never runs, three rules report
zero calls, and `psql` finds no rows).

No sleeps: the last mock rule is the result of the model's last statement, and each statement
autocommits, so once `wait_for_mocks` returns the server holds everything.

## `e2e_test.rs` — the follow-up depth bound (same-project)

`a_query_result_chain_is_followed_and_bounded` points the client at NetGet's own PostgreSQL
server and answers **every** `postgresql_query_result` with another `SELECT`. The server must
see exactly five queries and the client exactly five result events: one from the connect
event plus four follow-ups, the fifth result's answer dropped at `MAX_FOLLOWUP_DEPTH`.
Verified by removing the bound, at which point the chain runs until the test gives up.

The three `#[ignore]`d no-mock tests that used to sit beside it
(`test_postgresql_client_connect_and_query`, `…_llm_controlled_queries`, `…_transactions`)
were deleted: each asserted only that the word "connected" appeared, against NetGet's own
server, and the real-server test covers connecting and querying against a real one.

## `command_channel_test.rs` (0 calls)

An `execute_query` injected through `AppState::send_to_client` (the dashboard's `[ send ]`)
reaches a NetGet PostgreSQL server with a `*` static handler.

## Not covered

- TLS, and password/SCRAM authentication (the test cluster uses `trust`).
- A query the server rejects: it is logged, and the model is not told.
- Types beyond bool/int/float/text, which reach the model as `null`.
