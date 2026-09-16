# MySQL Protocol Implementation

MySQL wire protocol server built on `opensrv-mysql` v0.7. The library owns the
handshake, packet framing and command parsing (`AsyncMysqlShim`); the LLM owns
the answer to every query. **There is no database** — no tables, no rows, no
storage of any kind in Rust. The only per-connection state is the prepared
statement text map the wire protocol requires.

**State**: Experimental — LLM-authored, not human-reviewed. Result sets, OK
packets and ERR packets verified against the real `mysql` CLI (8.0).
**Port**: 3306 by default. **Privilege**: `None` (3306 > 1024).
**Stack**: `ETH>IP>TCP>MySQL`.

## What the model sees and controls

**Event**: `mysql_query`, fired for `COM_QUERY` and for `COM_STMT_EXECUTE`
(the stored statement text is replayed as if it were a fresh query).

| Field | Notes |
|---|---|
| `query` | the SQL text |

`client_ip`, `client_port`, `connection_id` and `server_id` are added by the
event logger, so log templates may reference them.

**Actions** (all sync; there are no async actions):

| Action | Parameters | Wire form |
|---|---|---|
| `mysql_query_response` | `columns` (required), `rows` (required) | result set |
| `mysql_ok_response` | `affected_rows`, `last_insert_id` (both optional) | OK packet |
| `mysql_error_response` | `error_code` (required), `message` (required) | ERR packet |
| `close_this_connection` | — | answers the current query, then drops the connection |

`columns` is an array of `{"name": …, "type": …}`. Recognised type names —
`INT`/`INTEGER`, `BIGINT`, `SMALLINT`, `TINYINT`, `FLOAT`, `DOUBLE`, `DECIMAL`,
`DATE`, `TIME`, `DATETIME`/`TIMESTAMP`, `BLOB`/`BINARY`, `TEXT`, `VARCHAR` —
set the column metadata *and* the encoding.

**The declared type is not cosmetic, and believing it was cost the binary
protocol entirely.** `COM_QUERY` uses MySQL's text protocol, where opensrv-mysql
writes every cell as a length-encoded string whatever the column says — so
handing it a `String` worked. `COM_STMT_EXECUTE` (every prepared statement) uses
the **binary** protocol, where opensrv encodes according to `Column::coltype` and
returns `io::Error` for a Rust type it cannot write as that type. `String` is
rejected by every numeric and every temporal column, and that error ends the
*session*. The protocol's own advertised example,
`{"name": "id", "type": "INT"}`, therefore killed the connection on
`conn.exec(...)` while working perfectly on `conn.query(...)`.

`write_cell` now encodes each value as its column demands: integer columns take
an integer, `FLOAT`/`DOUBLE` a float, `DATE`/`TIME`/`DATETIME`/`TIMESTAMP` a
parsed `YYYY-MM-DD` / `HH:MM:SS` / `YYYY-MM-DD HH:MM:SS` string, everything else
a string. A value that cannot be represented as its column's type is sent as
**SQL NULL with a WARN** — one odd cell must not cost the connection, and a
silently coerced wrong number would be worse than an explicit absence. (`TIME`
is additionally bounded at 34 days, because opensrv's `Duration` encoder holds
`assert!(d <= 34)` and the value comes from model output.)

JSON `null` is a real SQL NULL (the NULL bitmap in binary, `0xFB` in text). It
used to be written as the four-character string `"NULL"`, which every client read
as data.

**Rows are padded and truncated to the column count.** opensrv returns
`InvalidData` from `end_row()` for a short row and that error ends the session,
so a model that miscounted one row used to kill the connection instead of
producing one odd row. PostgreSQL's handler has always padded; MySQL now matches
it, with a WARN naming the mismatch.

### Error codes

`opensrv_mysql::ErrorKind::from(u16)` **panics** on any code outside its table
(`opensrv-mysql-0.7.0/src/errorcodes.rs:2807`). Because the code comes from model
output, `mysql_error_kind()` accepts a fixed list and falls back to 1105
(`ER_UNKNOWN_ERROR`) with a WARN rather than calling `From` directly:

> 1044, 1045, 1046, 1049, 1050, 1051, 1052, 1054, 1062, 1064, 1065, 1136, 1146,
> 1149, 1216, 1217, 1364, 1451, 1452, 1690

The message is always sent verbatim; only the numeric code and SQLSTATE fall
back. Verified: `mysql_error_response` with 1146 arrives as
`ERROR 1146 (42S02) … Table 'd.users' does not exist`; with 987654 it arrives as
`ERROR 1105 (HY000)` and the server stays up.

### Failure behavior

- **No response action** → ERR packet `1105` / `HY000` carrying the `WireFailure` category,
  logged at WARN with `decision=fail_closed_no_action`. It used to be an empty OK packet, which
  a driver reads as a statement that ran and affected zero rows — so an INSERT/UPDATE/DELETE the
  model declined was reported as completed. 1105 and not 1205, because nothing timed out here
  and a retryable code would invite the client to try again.
- **EXECUTE against an unknown or already-closed statement id** →
  `ER_UNKNOWN_STMT_HANDLER` (1243). It used to be an OK packet, so a client reusing a stale
  handle after a reconnect saw a successful write that never ran.
- **LLM call fails** → ERR packet `1105` / SQLSTATE `HY000`, message
  `netget: <error>`. (It used to send an empty OK, which the client could not
  distinguish from success.) When `crate::llm::is_overload_error` says the
  failure was capacity exhaustion the code is `1205`
  (`ER_LOCK_WAIT_TIMEOUT`, also `HY000`) instead — the code drivers already
  treat as transient and safe to retry, rather than 1105's permanent server
  fault. Covered by `tests/server/mysql/llm_failure_test.rs`.
- **More than 4096 live prepared statements on one connection** →
  `ER_MAX_PREPARED_STMT_COUNT_REACHED`. The map is only pruned by
  `COM_STMT_CLOSE`, so without the cap a PREPARE loop would grow it unbounded.

## Dashboard injection & connection counters

**No peer handle — the dashboard cannot message or disconnect a MySQL peer.**

A correction to what this section used to say, because the reason matters elsewhere in
this file: `run_on` does **not** have to be given the write half. Its `W` is only
`AsyncWrite + Send + Unpin`, which `&mut WriteHalf<TcpStream>` satisfies, so the write
half can be *lent* for the session and taken back when the crate's loop gives up. That
is exactly what the packet bound below does to answer with an ERR packet. What it does
not do is make a peer handle possible, and the three reasons are unchanged — they are
about writing *during* a session, not after one:

1. opensrv owns the socket and the packet **sequence-id counter** for the whole
   session; an unsolicited packet written from outside would desync it.
2. MySQL is strictly request/response — the server must not emit a packet the
   client did not ask for.
3. All wire verbs (`mysql_query_response`, `mysql_ok_response`,
   `mysql_error_response`) return `ActionResult::Custom` and are encoded by a
   `QueryResultWriter<'a, W>` that only exists *inside* an `on_query`/`on_execute`
   callback. The generic peer task can only execute `Output`/`CloseConnection`,
   so even with a write half it could not encode a MySQL response.

Consequently the dashboard shows the dim "cannot message or disconnect a peer
from here yet" row for MySQL connections, and there is no `close_connection`
arm in `execute_action` (nothing out-of-band can reach it).

**Connection counters are still updated.** opensrv hides the raw byte streams,
but the shim sees the application-visible payload at each command boundary, so
`handle_query`, `on_prepare` and `on_init` call
`AppState::update_connection_stats` with the SQL/DB text received and an estimate
of the response payload produced (see `MysqlHandler::record_stats`). These are
payload-size approximations, **not** exact wire bytes — opensrv adds a 4-byte
packet header and text-protocol framing on top — but they keep the rail's `↓/↑`
counters live and refresh `last_activity`. Proven with zero LLM calls by
`tests/server/mysql/connection_stats_test.rs`.

## Architecture

- `spawn_with_llm_actions` binds with `?` (so a bind failure surfaces as
  `ServerStatus::Error`) and registers the accept-loop `JoinHandle` via
  `AppState::register_server_task()` so `stop_server` releases the socket.
- One task per connection running `AsyncMysqlIntermediary::run_on` over a
  `tokio::io::split()` stream. On exit the connection is marked `Closed` in
  `AppState`.
- `close_this_connection` is implemented by writing the current response and then
  returning `io::ErrorKind::ConnectionAborted` from the shim, which is the only
  way to stop the loop opensrv drives.
- The per-connection task is registered with `register_server_task` as well as
  the accept loop. Aborting only the accept loop releases the port but leaves
  every in-flight session running, so `stop_server` did not actually stop the
  server. `register_server_task` prunes finished handles on each call, so the
  vector cannot grow without bound.
- PREPARE stores the SQL text keyed by an incrementing statement id; EXECUTE
  looks it up and re-runs it as a query; CLOSE removes it.
- **PREPARE reports the statement's `?` count** (`count_placeholders`, which
  skips `?` inside string literals, quoted identifiers and comments). The count
  is not cosmetic: the client stores it and uses it to frame COM_STMT_EXECUTE.
  Replying "no parameters" unconditionally — as this did — meant
  `conn.exec("… WHERE id = ?", (42,))` was rejected client-side and never reached
  the wire at all. Statements past `MAX_PLACEHOLDERS` (1024) get
  `ER_PS_MANY_PARAM` rather than a silently wrong count.
- Parameter **values** are still not substituted — the model sees the statement
  text with its `?` placeholders intact — and `on_execute` deliberately does not
  iterate `ParamParser`. opensrv-mysql's `params.rs` panics on several malformed
  COM_STMT_EXECUTE shapes, including an explicit `panic!("bad column type")` on a
  client-chosen byte (see IMPROVEMENTS' panic audit), so decoding parameters
  would trade a limitation for a remotely reachable panic.

## Not implemented

- **Authentication** — every connection is accepted; username and password are
  ignored. Note the MySQL 9.x client no longer ships `mysql_native_password`, so
  test with an 8.0 client (`/opt/homebrew/opt/mysql@8.0/bin/mysql`) or
  `mysql_async`.
- **TLS**.
- **Prepared-statement parameter values** — see above; the `?` reaches the model
  unsubstituted.
- **Binary column data** — `BLOB`/`BINARY` values are sent as UTF-8 text.
- **Transactions, stored procedures, multi-statement queries** — `BEGIN`,
  `COMMIT` and friends reach the model as ordinary queries.
- **`SELECT @@version` and other system variables** — the model must be told to
  answer them; nothing is auto-generated.

## Testing

`tests/server/mysql/test.rs` (note: `test.rs`, not `e2e_test.rs`), declared in
`tests/server/mod.rs`.

```bash
./cargo-isolated.sh test --no-default-features --features mysql \
    --test server::mysql::test -- --test-threads=100
```

Real-client check used during review, with a static handler so no model is
involved:

```bash
netget --mcp-http 18899 &
# start_server protocol=mysql port=13306 event_handlers=[{mysql_query → static …}]
/opt/homebrew/opt/mysql@8.0/bin/mysql -h 127.0.0.1 -P 13306 -u root \
    --protocol=TCP -e "SELECT * FROM t"
```

## Example prompts

```
Start a MySQL server on port 3306 for a database with a users table
(id INT, email VARCHAR). Answer SELECT with mysql_query_response, answer
INSERT/UPDATE/DELETE with mysql_ok_response, and answer a query naming an
unknown table with mysql_error_response error_code 1146.
```

## References

- [MySQL client/server protocol](https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_connection_phase.html)
- [opensrv-mysql](https://docs.rs/opensrv-mysql/)
- [mysql_async](https://docs.rs/mysql_async/) — used by the E2E tests

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever, and a
hundred of them was a free denial of service on a server that would happily accept a hundred
more. It now declares both halves; the constants and the reasoning live beside them in
`src/server/mysql/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `HANDSHAKE_RESPONSE_TIMEOUT` | 30s | MySQL is server-speaks-first: `opensrv-mysql` writes the initial handshake and a real client answers at once, with the credentials already in hand. This bounds "connected, took the greeting, said nothing". |
| `IDLE_BETWEEN_COMMANDS_TIMEOUT` | 600s | Real MySQL's `wait_timeout` defaults to **eight hours**, a bound in name only, so copying it was not an option the way copying Kafka's was. Ten minutes is safe because of what this server is: it holds no session state a reconnect would lose (the no-storage rule), so a reaped pooled connection costs `mysql_async` or a JDBC pool one transparent reconnect. |
| `MAX_CONNECTIONS` | 256 | Refusal: **an ERR packet carrying error 1040 `ER_CON_COUNT_ERROR`, SQLSTATE `08004`** — precisely what a real MySQL server sends, in precisely this position, in place of the initial handshake as packet sequence 0. `mysql_async` surfaces "Too many connections" and the CLI prints `ERROR 1040 (08004)`. |
| `packet_limit::MAX_PACKET_BYTES` | 67108864 | The number the server **publishes**. `opensrv-mysql` answers `SELECT @@max_allowed_packet` with 67108864 itself, without consulting the shim, and never compared anything against it — a published ceiling that is not applied is worse than none, because a client sizes its writes by it. Enforcing anything *smaller* would leave the same mismatch pointing the other way. Refusal: **error 1153 `ER_NET_PACKET_TOO_LARGE`, SQLSTATE `08S01`**, with the message text mysqld itself uses. |

**The deadline covers the read and nothing else.** `AsyncMysqlIntermediary::run_on` owns the protocol loop, so there is no `read()` of ours to wrap — but it takes a *generic* reader, which is the seam. `IdleTimeoutReader` arms its clock only while a read is actually outstanding, so while `MysqlHandler` is working the reader is not being polled and no clock runs. The LLM round-trip, and a `manual`
rule parking an event for a human (`src/state/intercepts.rs`, 300s by default), are outside
every deadline here, so an answer that takes minutes can never close the connection it is an
answer for. That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse:
TFTP evicted live transfers because "idle" was measured wrongly.

`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is removed;
`tests/accept_bounded_test.rs` drives the shared helper, including the guarantee that a busy
connection is never reported as idle.

### The packet bound: where it sits, and the seam that lets it answer

`opensrv-mysql` 0.7.0 has **no maximum-packet check anywhere**. `PacketReader::next_async`
grows its buffer by `max(1 MiB, len * 2)` until a whole *logical* packet fits, and `packet()`
assembles that logical packet with `nom::multi::fold_many0(fullpacket, …)` — an unbounded fold
over 16 MiB continuation fragments. So the peer decided how much this process allocated, and it
decided **before authentication and before any model call**: the first thing a client sends is
its handshake response, read through exactly this path.

Two seams make the repair possible, and neither is obvious from the crate's docs:

- **The reader.** `run_on` takes a *generic* `R: AsyncRead`, so
  `packet_limit::PacketLimitReader` sits beneath the crate and decides each packet from the
  length the peer **declared** in its 4-byte header — before the payload behind it is read, and
  therefore before anything is allocated for it. For a chain of `0xFFFFFF` fragments the bound
  is on the running sum, because that is what a logical packet's declared size *is*; a
  per-fragment check would pass every fragment forever.
- **The writer.** `run_on`'s `W` is `AsyncWrite + Send + Unpin`, which `&mut WriteHalf` meets,
  so the connection task lends the write half instead of giving it away. When the limiter trips,
  the crate's loop returns and the task still owns the socket — which is the only moment at
  which a refusal decided *beneath* the crate can be expressed *in* the crate's protocol.

The ERR packet carries sequence `refused + 1`. That is not a detail: MySQL numbers packets
within a command and a conforming client discards a reply out of sequence, so getting it wrong
turns a clear refusal back into the silence the bound exists to replace.

**It then drains before it closes** (`LINGER_DRAIN_BYTES` / `LINGER_DRAIN_TIMEOUT`, nginx's
`lingering_close`). A peer refused here is by definition still writing, and closing a socket
with unread data in the receive queue sends `RST`, which discards the bytes already written —
so without the drain the peer's `write` fails with `ECONNRESET` and it never reads the error
number, the SQLSTATE or the message.

`tests/server/mysql/packet_limit_test.rs` covers all of it: the framing decision at an exact
boundary and across a legitimate chain, an oversized packet sent **as the handshake response**
(so the bound is demonstrably pre-auth) answered 1153 with zero model calls, and an ordinary
query still working. Verified by removing the check, at which point three of the four fail and
the ordinary-query control still passes.
