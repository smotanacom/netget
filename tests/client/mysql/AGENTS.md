# MySQL Client E2E Tests

Three files, declared in `tests/client/mysql/mod.rs`. Nothing is `#[ignore]`d.

| File | Peer | Tests | LLM calls |
|---|---|---|---|
| `real_server_test.rs` | **real `mysqld`** + the `mysql` CLI | 1 | 6 |
| `e2e_test.rs` | NetGet's own MySQL server | 4 | 7 + three at `expect_at_least(0)` |
| `command_channel_test.rs` | NetGet's own MySQL server | 1 | 0 |

## Running

`--test` names a **target**, not a module path:

```bash
./cargo-isolated.sh test --no-default-features --features mysql \
    --test client -- client::mysql --test-threads=100
```

## `real_server_test.rs` — the evidence the rating rests on

The peer is Oracle's `mysqld`, set up per test by `tests/helpers/real_server.rs`:
`mysqld --no-defaults --initialize-insecure --datadir=<dir>/data` (a setup command; a
passwordless `root@localhost`), then `mysqld --no-defaults` on a probed loopback port with its
socket, pid file and data in the temp dir, `--mysqlx=OFF`, `--skip-log-bin`, and an
`--init-file` that creates the `netget_e2e` database. Ready when it logs `ready for
connections.`. State is read back with the `mysql` CLI over TCP. **It fails, never skips,**
when `mysqld` or `mysql` is missing, naming `brew install mysql` /
`apt-get install mysql-server mysql-client`.

`--no-defaults` must be the **first** argument, and it matters most on Ubuntu, whose
`/etc/mysql` config sends the error log to `/var/log/mysql` (so the readiness line would never
reach the helper) and pins the data directory and socket. Ubuntu's AppArmor profile for
`mysqld` also confines it to `/var/lib/mysql`; CI unloads it.

Measured locally (MySQL 9.3, x86_64 under Rosetta): initialisation ~2.5s, whole test ~3.5s.

### `mysql_client_creates_inserts_and_selects_against_mysqld` (6 calls)

The client connects with `startup_params` `{"username": "root", "database": "netget_e2e"}`.
`mysql_connected` → `CREATE TABLE notes`; its result (matched on the query and
`affected_rows` 0) → `INSERT … 'hello from the model'`; that result, matched on
`affected_rows` 1 **and** `last_insert_id` 1 → `SELECT id, body`; the SELECT's result (matched
on the query, `row_count` 1 and the body in `result`) → an `INSERT` of
`the model saw: <body> (id <id>)` built from the row; that result → nothing. The last rule is
listed first because its query also names `notes`. Then `mysql` must print exactly the two
bodies in order, and `information_schema` must show the model's column type.

### What it found

The result event reported the **row count** as `affected_rows`, so an `INSERT` read as having
affected nothing and a `SELECT` as having affected every row it returned. NetGet's own server
could not show it: nothing there compared the two. The client now reads `affected_rows()` and
`last_insert_id()` off the connection under the same guard as the query (they are overwritten
by the next one) and reports `query`, `row_count`, `affected_rows` and `last_insert_id`
separately. Verified by mutation: putting the row count back fails the INSERT rule.

### Why this is condition 4 of the client bar

The assertions are on the server's state, written by statements the model chose — one of them
built from rows it was shown. Verified by mutation: dropping the actions the model returns for
a `mysql_result_received` makes the test fail.

No sleeps: the last mock rule is the result of the model's last statement, with autocommit on,
so once `wait_for_mocks` returns the server holds everything.

## `e2e_test.rs` — same-project

The peer is NetGet's own MySQL server, so these show the two halves agree — circular evidence,
kept for what it does cover.

- `a_follow_up_query_from_the_model_actually_reaches_the_server` (7 calls) — the model's
  answer to a result is executed: the server must see two queries.
- `test_mysql_client_connect_and_query`, `test_mysql_client_with_database`,
  `test_mysql_client_transaction` — connect and issue queries with `expect_at_least(0)` on the
  model rules, so they assert little beyond a connection. `test_mysql_client_with_database`
  puts `username`/`database` at the top level of `open_client`, where they are **not** read
  (only a nested `startup_params` is); the real-server test is what exercises the `database`
  parameter.

## `command_channel_test.rs` (0 calls)

An `execute_query` injected through `AppState::send_to_client` (the dashboard's `[ send ]`)
reaches a NetGet MySQL server.

## Not covered

- TLS, and password authentication (the test server's `root` has no password, which
  `caching_sha2_password` answers on its fast path without the RSA exchange).
- A query the server rejects: it is logged and marks the client `Error`, and the model is not
  told.
- Prepared statements (the client uses the text protocol throughout).
