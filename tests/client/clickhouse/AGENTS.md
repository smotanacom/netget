# ClickHouse client tests

- `session_test.rs`: against NetGet's own server (its SQL script copied in): a refused login
  failing creation, the typed result, DDL, an INSERT the server's handler sees, an exception,
  and rows that do not fit refused with the session still usable.
- `real_server_test.rs`: the official ClickHouse 24.8 server, started by `RealServer` in a
  temporary directory with a minimal config on a probed port: a refused login, a MergeTree
  table created, typed rows (including Nullable, Date, DateTime, Bool and an Int64 past 2^53)
  inserted and read back, an aggregate, an exception.

`install_peers.py ROOT` downloads the release tarball by SHA-256, extracts the one binary, and
installs clickhouse-driver (hash-pinned), printing NETGET_CLICKHOUSE_BIN and
NETGET_CLICKHOUSE_PYTHON for both suites.
