# MSSQL Server Protocol Implementation

Microsoft SQL Server TDS 7.4 server. No Rust TDS *server* library exists
(`tiberius` is a client), so pre-login, login, packet framing and every response
token are built by hand. **There is no database** — the LLM answers every query.

**State**: Experimental — LLM-authored, not human-reviewed. The pre-login/login
handshake and the COLMETADATA/ROW/DONE token stream were decoded byte-by-byte
against MS-TDS during review; no SQL Server client was available on the review
machine, so `tiberius` interop rests on `tests/server/mssql/test.rs`.
**Port**: 1433 by default. **Privilege**: `None` (1433 > 1024).
**Stack**: `ETH>IP>TCP>TDS>MSSQL`.

## What the model sees and controls

**Event**: `mssql_query`, fired for SQL Batch (0x01) and for RPC (0x03) when SQL
text could be recovered from the packet.

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

`parse_rpc_request` does not decode the RPC header or its parameters. It scans
the packet on 2-byte boundaries, decodes each window as UTF-16LE, and returns the
first run that starts with `SELECT`/`INSERT`/`UPDATE`/`DELETE`/`CREATE`/`DROP`/
`ALTER`. A parameterised `sp_executesql` therefore reaches the model with its
`@P1` placeholders intact and its parameter values missing, and an RPC whose SQL
does not begin with one of those keywords yields an empty DONE.

## Architecture

- `spawn_with_llm_actions` binds with `?` (so a bind failure surfaces as
  `ServerStatus::Error` — confirmed: a busy port reports
  `Error: Address already in use`) and registers the accept-loop `JoinHandle` via
  `AppState::register_server_task()` so `stop_server` releases the socket.
- One task per connection. `handle_connection` wraps `run` so the connection is
  always marked `Closed` in `AppState` on exit; outbound bytes/packets are
  recorded per write.
- Pre-login advertises version 16.0.0.0 and ENCRYPT_NOT_SUP. Login is accepted
  unconditionally and answered with ENVCHANGE (database `master`, language
  `us_english`, packet size 4096), an INFO token and DONE.

## Not implemented

- **Authentication** — username, password and database are ignored. No NTLM, no
  Windows auth, no Azure AD.
- **TLS** — pre-login advertises ENCRYPT_NOT_SUP.
- **Prepared statements / RPC parameters** — see above.
- **Transactions, MARS, cursors, bulk load, `nvarchar(max)`, VARBINARY, XML,
  spatial and decimal precision/scale.**
- **`SELECT @@VERSION` and other system queries** — the model must be told to
  answer them.

## Testing

`tests/server/mssql/test.rs` (note: `test.rs`, not `e2e_test.rs`), declared in
`tests/server/mod.rs`.

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
