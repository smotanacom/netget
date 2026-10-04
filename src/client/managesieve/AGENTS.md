# ManageSieve client — Experimental

Uses `src/server/managesieve/proto.rs`. Reads the greeting's capabilities, requires SASL PLAIN,
logs in with an initial response (`user`, `password`, optional `authorize_as`) and reports
`managesieve_connected` (implementation, Sieve extensions, every capability). One command at a
time: list, get, put, check, set_active, delete, rename, have_space; scripts go as
non-synchronizing literals. Each answer is `managesieve_response` with the status, response code
(e.g. NONEXISTENT, ACTIVE, QUOTA/MAXSIZE), the server's message (a script's compiler errors),
and the script list or script text. `disconnect` sends LOGOUT. No STARTTLS.
