# OCPP charge point tests

`peer_test.rs` runs python ocpp 2.1.0's central system (independent, schema-validating, unchanged)
and lets NetGet's charge point walk BootNotification → Heartbeat → StatusNotification → Authorize →
Start/StopTransaction (1.6) or TransactionEvent (2.0.1) from its own handlers; the central system logs
every call it accepted and sends Reset after boot, which the charge point must accept. Needs
`NETGET_OCPP_PYTHON` from `tests/server/ocpp/install_peers.py`; fails without it.
