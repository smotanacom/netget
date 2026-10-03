# NUT UPS server

Feature `nut`, TCP/3493, RFC 9271 and the upstream NUT network protocol guide:
https://www.rfc-editor.org/rfc/rfc9271.html
https://networkupstools.org/docs/developer-guide.chunked/ar01s09.html

Implemented commands: LIST UPS/VAR/RW/CMD/ENUM; GET VAR/DESC/CMDDESC/UPSDESC/TYPE;
USERNAME, PASSWORD; authenticated SET VAR and INSTCMD; LOGOUT (DETACH alias), VER,
HELP. LF framing is emitted; CRLF input is tolerated. Double quotes and backslashes
are escaped and decoded. Identities and list delimiters come from the parsed request,
so a handler cannot reply to a different UPS or forge extra lines.

`nut_request` carries operation, optional ups/name/value, and the authenticated username
for write-policy decisions. `nut_reply` supplies entries, value, types, explicit ok=true,
or a recognized error. `nut_auth` carries credentials and only accepts
`nut_auth_decision {allowed: bool}`. There are no built-in accounts or UPS data. Failed,
missing, duplicate or invalid handler replies fail closed (DATA-STALE and close).
Terminal handler outcomes log `decision=model_answer`, `model_reject`, `model_silent`,
`fail_closed_llm_error` or `fail_closed_invalid_reply`; failed writes log
`fail_closed_send_error`. Logs identify the connection and operation without credentials
or backend details. Invalid actions fail closed even beside a usable reply.
Authentication failure denies protected operations; credential fields cannot be reset
within a connection. A successful SET or INSTCMD acknowledges the handler's decision,
not a hardware operation: handlers own simulated data/effects.

Plain TCP only. STARTTLS explicitly returns FEATURE-NOT-SUPPORTED. No TLS downgrade
is attempted by the client. ATTACH/LOGIN, PRIMARY/MASTER, FSD, NUMATTACH/NUMLOGINS,
LIST RANGE/CLIENT, protocol feature negotiation, tracking and optional INSTCMD data
arguments are unsupported and return UNKNOWN-COMMAND. This is a bounded management
subset, not a complete upsmon replacement or a hardware driver.

Bounds: 8192 wire bytes per line; 4096 list entries; 1 MiB rendered response; 256 live
connections; whole-command idle deadline 300s (startup override `idle_timeout_secs`, 1..=86400);
30s writes. The idle deadline covers drip-fed command bytes and excludes handler time
so manual handlers work. Every session and listener task is registered with AppState;
stop_server closes existing sockets and releases the listener. No shared I/O locks.

Maturity stays Experimental. Tests cover wire behavior, deterministic and mocked-model
handlers, denial and acceptance, bounds and shutdown. Independent peers are official NUT
2.8.4 upsc and upscmd, built from upstream source with SSL/hardware drivers disabled;
the client is tested against upsd 2.8.4 plus dummy-ups 0.22. See tests/server/nut/CLAUDE.md.
No packet-capture oracle or fuzz target was added, and untested commands are not implied
by successful read/instant-command tests.
