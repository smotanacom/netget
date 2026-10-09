# ClickHouse server (native TCP)

Hand-written over Tokio TCP (`wire.rs`, shared with the client). `opensrv-clickhouse` was the
obvious crate and was rejected: it allocates from wire-supplied lengths with no bound
(`vec![0; str_len]` straight from a VarUInt, and a read buffer that grows until a packet
parses), a pre-authentication DoS, and the native protocol has no frame length a guard in
front of it could check.

## What Rust owns

Protocol revision 54429 — the first that sends settings as strings, and before the later
revision-gated query fields (inter-server secret, OpenTelemetry, distributed depth,
parameters, the hello addendum). Both clickhouse-client 24.8 and clickhouse-driver negotiate
down to it. Rust handles hello and login (credentials, when configured, are checked in
constant time; a mismatch is exception 516), ping/pong, the query packet and its client info,
the empty external-tables block every query is followed by, Data blocks with typed columns
(integers, floats, Bool, String, Date, DateTime, Nullable of those), LZ4 compressed frames
with CityHash128 checksums when the query asks for compression, progress, the INSERT
exchange (header block, row blocks until the empty one) and end of stream.

## What the handler decides

`clickhouse_query` → `clickhouse_result {columns, rows}`, `clickhouse_ok`,
`clickhouse_insert {columns}` (the table shape the client encodes its rows against) or
`clickhouse_exception {code, message}`; then `clickhouse_insert_data {columns, rows}` →
`clickhouse_ok` or an exception. Settings the client sent are in the event.

## Failure modes and bounds

A handler failure, silence or wrong answer is an exception (1002, or 202 when the backend is
saturated) carrying `WireFailure`'s text, never a result, each with its `decision=` tag.
Every read is bounded where it happens: strings 1 MiB, a block 16 MiB including its
compressed frames, 1000 columns, 100000 rows (per block and per INSERT), 1000 settings, a bad
frame checksum ends the connection. Fields that must follow at once have 30 s; a connection
may be silent `idle_timeout_secs` (default 300) between packets.
