# NUT UPS client

Feature `nut` shares the strict RFC 9271 codec with the server. `nut_request` uses a
structured operation plus optional ups/name/value. Operations: list_ups/list_var/list_rw/
list_cmd/list_enum/get_var/get_desc/get_cmddesc/get_upsdesc/get_type/set_var/instcmd/
username/password/logout. `disconnect` closes immediately; logout performs the wire
exchange. GET/LIST row identities and END LIST must match the actual pending request.
Server ERR replies are structured events, not transport errors or fabricated successes.

`nut_connected` and `nut_response` use call_llm_for_client, including event_handlers
and budgets. Request/password response-event fields and injected logs redact passwords.
The server's authentication event intentionally contains the supplied credentials for
its policy handler; this client does not automatically read credentials from the host.

A registered I/O task owns the split socket and serializes actions from a bounded
injected-command queue and a bounded handler-action queue. A separately registered
handler dispatcher allows injection while a connected event is parked manually. NUT
has no request IDs, so at most one transaction is in flight. Injected disconnect remains responsive during a pending transaction; other injected
requests are explicitly rejected as busy until it finishes. Every written command gets an outcome;
invalid actions are rejected before bytes are written. Event overload closes with an
error rather than silently dropping responses. Queues hold at most 16 items.

The dispatcher drains final response events after a clean peer close, owns no socket,
and is cancelled when the client is removed. I/O errors cancel it immediately. All
background tasks are registered. Connection/read/write deadlines are 30s, with a whole
response deadline preventing drip-feed stalls. Lines are 8192 bytes, lists 4096 rows,
and complete responses 1 MiB. Partial EOF, malformed rows and correlation mismatches
fail the connection.

Plain TCP only: no STARTTLS, attachment/primary/FSD, tracking or hardware integration.
Maturity Experimental. Independent verification uses official upsd 2.8.4 with its
own dummy-ups driver, not netget's server; additional fixture tests cover fragmented
wire replies, command injection, errors and cleanup. See tests/client/nut/CLAUDE.md.
