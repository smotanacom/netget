# ClickHouse client (native TCP)

Uses the server's `wire.rs` at revision 54429. Logs in as `user`/`password` to `database`; a
refused login fails the connection with the server's code and message. Queries are sent
uncompressed (a client may choose), each followed by the empty external-tables block, and
read to end of stream: Data, Totals, Extremes and Log blocks, Progress, ProfileInfo,
TableColumns and Exception packets.

- `clickhouse_query {query}` → `clickhouse_result {ok, columns, rows}` or
  `{ok: false, exception: {code, message}}`.
- `clickhouse_insert {query, rows}`: the server's header block names the columns and types,
  the rows are encoded to match, and the outcome arrives as `clickhouse_result` with
  `rows_written`. Rows that do not fit end the insert with no data and are refused.

A result column of a type outside NetGet's set (arrays, decimals, LowCardinality, UUID…)
ends the session: its bytes cannot be skipped without knowing the layout. Same bounds as
the server.
