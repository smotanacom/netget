# ClickHouse server tests

`SQL_SCRIPT` (in `wire_test.rs`) answers SELECTs on `events` with every supported type,
accepts `INSERT INTO events` (refusing more than ten rows), accepts DDL and raises
UNKNOWN_TABLE (60) otherwise.

- `wire_test.rs`: raw packets — a refused and an accepted login, ping, the typed result
  uncompressed and compressed, DDL, an exception, a compressed INSERT whose rows the handler
  sees and one it refuses; then a hello string past 1 MiB, an INSERT block past 100000
  rows, a corrupted frame checksum, an idle connection, and no handler (a generic
  exception, never a result).
- `real_client_test.rs`: the official clickhouse-client 24.8 with `--compression 0` and `1`
  (JSONCompact output asserted whole), an INSERT with inline VALUES and an exception;
  Python clickhouse-driver for the result, an INSERT, settings and the exception. The client
  helper keeps the process's output and prints it if it ever runs past its deadline.

Peers from `tests/client/clickhouse/install_peers.py`. No LLM calls.
