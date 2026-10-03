# NUT client validation

`session_test.rs` uses a separately hand-written wire fixture, with literal request and
reply strings. It proves static connected/response handlers, fragmented list parsing,
injected queries, error events, injection rejection and socket cleanup. These fixtures
are independent readings of the wire format, not third-party interoperability evidence.

`real_server_test.rs` launches official upsd 2.8.4 and dummy-ups 0.22 in a private
TempDir, with synthetic UPS values. A readiness query checks the driver has populated
the real daemon before the netget client reads battery.charge and LIST VAR. Child
processes use kill_on_drop and are killed/reaped after success. Configuration and data
are temporary; no system NUT service or hardware is touched. Missing peers hard-fail.

Peer bootstrap, environment variables and the shared server/client test invocation are
in tests/server/nut/CLAUDE.md. The daemon and
driver tests cover real reads; authentication and SET/INSTCMD client emission have
codec/server-session coverage but are not asserted against a real write-capable UPS.
