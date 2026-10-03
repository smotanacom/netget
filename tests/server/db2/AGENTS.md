# Db2 (DRDA) Server E2E Tests

`drda_test.rs` + `e2e_test.rs`, declared in `tests/server/mod.rs`
(`pub mod db2;` → `tests/server/db2/mod.rs`).

## Strategy — byte-literal, NOT real-client

A real Db2 driver is very unlikely to be available on macOS, so the evidence is
**byte-literal**: the hand-rolled DRDA/DDM codec is asserted against spec-derived
constant byte strings, and the running server is driven with DRDA bytes built from
that (independently validated) codec. This proves the encoder is self-consistent
and matches the documented wire layout; it is **not** proof of interoperability
with a genuine Db2 connector. State this plainly anywhere results are summarised.

## `drda_test.rs` — pure codec (no server, no LLM)

Byte-literal assertions with hardcoded expected bytes:

- DDM object header (`length | codepoint`) and DSS envelope layout
  (`length | 0xD0 | format | correlator`), including the chaining flags in the
  format byte's high nibble.
- `parse_dss` round-trips a SECCHK carrying an EBCDIC USRID, and reports
  `Truncated` / `BadMagic` on malformed input.
- IBM037 EBCDIC known values and `"SELECT 1"` round-trip.
- `sqlcard_success` = the 5-byte null SQLCA; `sqlcard_error` carries SQLCODE
  (i32 BE) + SQLSTATE (EBCDIC) with the extended group NULL.

8 tests, no LLM calls, sub-millisecond.

## `e2e_test.rs` — handshake over real TCP (LLM mocked)

`test_db2_handshake_and_statement` opens a real TCP connection and walks the full
DRDA handshake, asserting each reply's code point and severity:

`EXCSAT→EXCSATRD`, `ACCSEC→ACCSECRD`, `SECCHK→SECCHKRM` (severity INFO / code
SUCCESS, because the mock accepts), `ACCRDB→ACCRDBRM` (INFO once authenticated),
then `EXCSQLIMM` with an embedded `SQLSTT` → `SQLCARD` whose body is the single
`0xFF` null-SQLCA success indicator.

## `peer_inject_test.rs` — dashboard peer injection (zero LLM calls)

`injected_close_connection_sends_eof_and_counters_move` starts a Db2 server with
no handlers, connects a raw `TcpStream`, waits for `has_peer_handle`, then:
`EXCSAT→EXCSATRD` (no LLM) and asserts the connection counters moved on both
directions; `send_to_peer` of `db2_accept_connection` → `Executed` (the wire verbs
are correlator-bound `Custom` results, so nothing is written); `send_to_peer` of
`close_connection` → `Disconnected`, the socket reads EOF, and the peer handle is
removed. It does **not** assert that an unknown action name errors: the shared
executor reports unknown names as executed, which is outside this protocol.

## LLM call budget (mocked)

`test_db2_handshake_and_statement`: startup + `db2_connect` (accept) + `db2_query`
(ok) = **3** calls. Total suite **3** (well under budget). Ends with
`server.verify_mocks().await?`.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features db2 \
    --test server -- db2:: --test-threads=100
```

## Not covered

SELECT result-set retrieval (OPNQRY/QRYDTA — not implemented), the error SQLCARD
path end-to-end through the server, prepared statements, and TLS. See
`src/server/db2/CLAUDE.md`.
