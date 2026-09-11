# MSSQL Server Protocol Implementation

Microsoft SQL Server TDS 7.4 server. No Rust TDS *server* library exists
(`tiberius` is a client), so pre-login, login, packet framing and every response
token are built by hand. **There is no database** — the LLM answers every query.

**State**: Beta — `actions.rs` says `DevelopmentState::Beta` and this file used
to say `Experimental` two screens away from it. Beta is the correct one: five
tests drive `tiberius`, a real independent TDS client, through login and queries,
none `#[ignore]`d and none skipped when something is absent. Not Stable — that
additionally wants spec compliance and scripting support reviewed, which has not
been done. The pre-login/login handshake and the COLMETADATA/ROW/DONE token
stream were separately decoded byte-by-byte against MS-TDS during review.
**Port**: 1433 by default. **Privilege**: `None` (1433 > 1024).
**Stack**: `ETH>IP>TCP>TDS>MSSQL`.

## What the model sees and controls

**Events**: `mssql_login`, fired once per LOGIN7 (see "Not implemented"), and
`mssql_query`, fired for SQL Batch (0x01) and for RPC (0x03) when SQL text could
be recovered from the packet — but only once the session has been admitted.

| Field | Notes |
|---|---|
| `query` | the SQL text |

**Actions** (all sync; there are no async actions):

| Action | Parameters | Tokens sent |
|---|---|---|
| `mssql_query_response` | `columns` (required), `rows` (required) | COLMETADATA + ROW* + DONE |
| `mssql_ok_response` | `rows_affected` (optional) | DONE with DONE_COUNT |
| `mssql_error_response` | `error_number` (required), `message` (required), `severity` | ERROR + DONE |
| `close_this_connection` | — | answers the query, then closes |

### Column types

`columns` is an array of `{"name": …, "type": …}`. Every column is emitted as one
of TDS's **nullable** (variable-length) types, because those are the only ones
whose COLMETADATA carries a length byte and whose row values carry a length
prefix — which is the shape this encoder writes.

| Type name | TDS type | Metadata length | Row encoding |
|---|---|---|---|
| `TINYINT` | INTNTYPE 0x26 | 1 | 1-byte length + int |
| `SMALLINT` | INTNTYPE 0x26 | 2 | 1-byte length + int LE |
| `INT`, `INTEGER` | INTNTYPE 0x26 | 4 | 1-byte length + int LE |
| `BIGINT` | INTNTYPE 0x26 | 8 | 1-byte length + int LE |
| `BIT`, `BOOL`, `BOOLEAN` | BITNTYPE 0x68 | 1 | 1-byte length + 0/1 |
| `FLOAT`, `REAL`, `DOUBLE`, `DECIMAL`, `NUMERIC`, `MONEY` | FLTNTYPE 0x6D | 8 | 1-byte length + f64 LE |
| anything else (`NVARCHAR`, `VARCHAR`, `TEXT`, …) | NVARCHARTYPE 0xE7 | 8000 bytes | USHORT length + UTF-16LE |

JSON `null` becomes a real SQL NULL (length 0, or 0xFFFF for NVARCHAR). A row
shorter than the column list is padded with NULLs and extras are dropped, because
TDS requires exactly one value per described column. NVARCHAR values are
truncated at 4000 UTF-16 code units — `nvarchar(max)` would require PLP-chunked
row values, which this encoder does not produce.

The previous mapping handed out FIXEDLENTYPE codes (INT4TYPE 0x38, INT8TYPE 0x7F,
BITTYPE 0x32, FLT4TYPE 0x3B) while still writing length bytes, so every non-string
column put structurally invalid tokens on the wire. `VARCHAR` mapped to 0xA7
(non-Unicode) but was written as UTF-16. NULL was written as the four-character
string `"NULL"`.

DONE tokens set DONE_COUNT (0x0010); without it the client ignores DoneRowCount
entirely, which silently discarded the `rows_affected` the LLM supplied.

### Failure behavior

TDS clients block until a DONE or an ERROR token arrives, so every branch below
writes one. What is written carries a *category*, never netget's own error text:
the error itself goes to the log and the status stream only
(`crate::utils::WireFailure`).

The three outcomes are distinguishable in the log by a stable `decision=` tag, so
a deliberate refusal is never confused with netget failing to obtain an answer:

- **The model refused** (`mssql_error_response`) → its own ERROR token, logged
  `decision=model_reject`.
- **No response action** → ERROR 50000 severity 16, message
  `netget: request could not be processed`, logged `decision=fail_closed_no_action`.
  Not a bare DONE: in SQL that reads as "ran, matched nothing", which is a
  successful answer to a query nobody answered. The one exception is an explicit
  `close_this_connection` with no other action — a decision rather than silence —
  which still sends DONE and logs `decision=model_close`.
- **LLM call fails** → ERROR severity 16 logged `decision=fail_closed_llm_error`:
  49918 ("not enough resources", on SqlClient's transient-retry list) when the
  backend is overloaded, 50000 otherwise, with the fixed `netget: …` category
  message for that class.
- **A second response action after the first** → logged at WARN and dropped; TDS
  allows only one token stream per statement.
- **Action result the handler does not recognise** → logged at WARN and skipped.
- **TDS packet length below 8** → connection closed.
- **Bulk Load (0x0E) or an unknown packet type** → ERROR 40002.

## Packet framing

8-byte header: `type | status | length (u16 big-endian) | SPID | packetID |
window`, then payload.

Outbound messages are split into `TDS_PACKET_SIZE` (4096) byte packets, with
status 0x00 on every packet but the last and 0x01 (EOM) on the last. A single
oversized write used to wrap the u16 length field and emit a packet declaring a
length far shorter than its payload.

Handled inbound packet types: 0x12 pre-login, 0x10 login, 0x01 SQL batch, 0x03
RPC, 0x0E bulk load (rejected), 0x07 attention (ends the connection).

### RPC parsing is heuristic

`parse_rpc_request` does not decode the RPC header or its parameters. It decodes
the whole payload once as UTF-16LE and returns the text from the earliest
occurrence of `SELECT`/`INSERT`/`UPDATE`/`DELETE`/`CREATE`/`DROP`/`ALTER` up to
the first non-printable character. A parameterised `sp_executesql` therefore
reaches the model with its `@P1` placeholders intact and its parameter values
missing, and an RPC containing none of those keywords yields an empty DONE.

Two things about it are load-bearing rather than incidental, and both were wrong:

- **One decode, not one per offset.** It used to slide a 2000-byte window forward
  two bytes at a time, decoding and upper-casing at every position — about 32,000
  windows for a maximum-size TDS packet, each allocating two strings and running
  eight substring searches, with nothing to yield the tokio worker. A single
  64 KB packet was seconds of CPU, repeatable by one connection. Every window sat
  on the same UTF-16 code-unit grid, so one decode covers identical text.
- **ASCII case folding, never `to_uppercase`.** The keyword offset came from the
  upper-cased copy and was used to slice the *original*. Unicode case mapping
  changes byte length (U+FB01 `ﬁ` → the two bytes `FI`), so a payload with one
  such character ahead of the keyword sliced mid-character and **panicked** — from
  the wire, and the panic skipped `handle_connection`'s teardown, leaving the
  connection `Active` in the dashboard for the life of the process.
  `tests/server/mssql/hostile_input_test.rs` covers it.

## Architecture

- `spawn_with_llm_actions` binds with `?` (so a bind failure surfaces as
  `ServerStatus::Error` — confirmed: a busy port reports
  `Error: Address already in use`) and registers the accept-loop `JoinHandle` via
  `AppState::register_server_task()` so `stop_server` releases the socket.
- One task per connection. `handle_connection` wraps `run` so the connection is
  always marked `Closed` in `AppState` on exit. Both directions are recorded:
  inbound per TDS packet read, outbound per packet written. Only the outbound
  side used to be counted, so the rail's `↓` sat at zero for every connection.
- Pre-login advertises version 16.0.0.0 and ENCRYPT_NOT_SUP. An **admitted**
  login is answered with ENVCHANGE (database from `mssql_login_ack`, default
  `master`; language `us_english`; packet size 4096), an INFO token and DONE —
  see "Not implemented" below for who decides, and note that admission is not the
  default.
- **A SQL Batch (0x01) or RPC (0x03) before any LOGIN7 is refused** with 18456
  severity 14, logged `decision=fail_closed_not_logged_in`, and the connection
  closes. The dispatch loop used to treat packet types as independent, so a peer
  that never sent a LOGIN7 had its statements answered and `mssql_login` never
  fired — the admission decision was skippable by declining to ask for it. Real
  drivers always log in; a hostile peer has no reason to.

## Not implemented

- **Password verification** — no NTLM, no Windows auth, no Azure AD, and the password in the
  LOGIN7 packet is never checked (or even parsed: it is deliberately not put into the event,
  because that would send a credential to the model and into the event log).

  The *decision*, however, is now the model's. Every TDS Login used to be accepted
  unconditionally — there was no `mssql_login` event and no action that could decline one, so
  an instruction like "only allow the user `reporting`" could not be enforced and the model was
  never asked. Authentication decided by default is the pattern the root `CLAUDE.md` calls the
  most dangerous in this codebase.

  `mssql_login` now fires with `username`, `database` and `app_name` parsed from LOGIN7
  (MS-TDS 2.2.6.4). Admission requires an explicit `mssql_login_ack`; `mssql_error_response`
  refuses with the model's own error. Three things refuse, kept apart in the log because the
  wire can only carry one error number: `decision=model_reject`,
  `decision=fail_closed_no_action` (the handler ran and produced neither verdict) and
  `decision=fail_closed_llm_error`. All three send SQL Server's own 18456 "Login failed for
  user" at severity 14, which every driver already maps to an authentication failure, and then
  close — TDS cannot continue a session whose login failed.

  A malformed or truncated LOGIN7 yields empty strings rather than an error, so an unparseable
  login still reaches the model and is refused on the same no-answer path. It must never be the
  reason a login is *granted*.

  Note for tests: a server that only mocks `mssql_query` no longer gets a session at all. Every
  suite here answers `mssql_login` with `mssql_login_ack` explicitly.
- **TLS** — pre-login advertises ENCRYPT_NOT_SUP.
- **Prepared statements / RPC parameters** — see above.
- **Transactions, MARS, cursors, bulk load, `nvarchar(max)`, VARBINARY, XML,
  spatial and decimal precision/scale.**
- **`SELECT @@VERSION` and other system queries** — the model must be told to
  answer them.

## Testing

`tests/server/mssql/test.rs` (note: `test.rs`, not `e2e_test.rs`),
`llm_failure_test.rs` and `hostile_input_test.rs`, all declared in
`tests/server/mssql/mod.rs`.

The first two drive `tiberius`, which is the Beta evidence: a real, independent
TDS client completing login and queries, not `#[ignore]`d and not skipped when
anything is missing (`tiberius` is a plain optional dependency the `mssql`
feature turns on, so it compiles wherever the feature does).

`hostile_input_test.rs` speaks raw TDS on purpose. Both defects it covers are
*unreachable through `tiberius`* — a conforming driver always logs in and never
emits a ligature — which is why a suite made entirely of real-client tests
missed them. A real-client test proves interoperability; it proves nothing about
what a peer that is not trying to interoperate can do.

```bash
./cargo-isolated.sh test --no-default-features --features mssql \
    --test server::mssql::test -- --test-threads=100
```

**Gap**: every case in that file — including `test_mssql_multi_row_query` —
responds with `mssql_ok_response`. The entire COLMETADATA/ROW encoding path is
uncovered. It was verified during review with a raw socket that performs
pre-login and login, sends a SQL batch, and decodes the token stream:

```
columns: id type=0x26 len 4 | big type=0x26 len 8 | flag type=0x68 len 1
         score type=0x6d len 8 | name type=0xe7 len 8000
rows:    [[1, 9007199254740991, True, 3.5, 'Alice'], [2, None, False, None, None]]
DONE     status=0x0010 curcmd=0x00c1 rowcount=2, no trailing bytes
```

## Example prompts

```
Start an MSSQL server on port 1433. Answer SELECT with mssql_query_response
using INT for numeric columns and NVARCHAR for text, answer INSERT/UPDATE with
mssql_ok_response, and answer a query against an unknown object with
mssql_error_response error_number 208 severity 16.
```

## References

- [MS-TDS] Tabular Data Stream Protocol, Microsoft Open Specifications
- [tiberius](https://docs.rs/tiberius/) — the TDS client used by the E2E tests
