# InfluxDB client checks

Mandatory decoder-backed independent receiver, with all five field kinds, escaping, four
timestamp precisions and gzip. Validate HTTP v2 route/query/auth/content-type/content-encoding
and require the peer's typed observations and status response. Fail if the pinned peer is
absent; never return early or mark ignored.

Exercise native pair handlers, explicit partial/error responses, atomic rejection before
socket creation, command injection during parked handlers, in-flight disconnect/removal,
response-body/time limits, event/action/follow-up bounds and shared-memory refresh. Cleartext
HTTP only. Mandatory official InfluxDB 2.9.1 service fixture on Linux/macOS (no missing-peer
skip), isolated data/telemetry disabled, all five kinds/four precisions/gzip writes followed
by official Python readback and actual401/404 errors. Daemon bootstrap contract and exact
environment are documented in tests/server/influxdb/CLAUDE.md. Windows has native and
decoder-backed evidence only. Service bootstrap fails explicitly on unsupported platforms.
