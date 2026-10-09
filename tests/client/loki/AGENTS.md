# Loki client checks

Require the pinned official Loki3.7.8 service on Linux/macOS; all three native carriers must
produce exact readback timestamps, labels, lines and structured metadata, with tenant
isolation and actual401/future-timestamp400 outcomes. Missing peer fails, never skips. Native
pair/injection/error checks cover atomic validation before socket creation, parked handlers,
response/action/depth bounds, malformed/oversized responses,260 and retry advice without
retries, single in-flight write, disconnect/removal cancellation and exchange deadline.

Bootstrap/env/platform/license contracts are in tests/server/loki/AGENTS.md. Linuxamd64 and
macOSarm64 service fixtures use isolated process/data/telemetry settings. Other-platform
native/Python checks do not claim service interoperability. Keep maturity Experimental.
