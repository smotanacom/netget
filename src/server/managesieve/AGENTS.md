# ManageSieve server — Experimental

RFC 5804. `proto.rs` parses command lines incrementally (atoms, numbers, quoted strings with
`\"`/`\\` escapes, `{n}` and `{n+}` literals that may span reads) and writes strings and status
lines; `mod.rs` is the session.

Rust owns:
- The greeting and CAPABILITY: IMPLEMENTATION and SIEVE from the startup parameters, SASL PLAIN,
  VERSION 1.0. STARTTLS is not advertised and answered NO.
- AUTHENTICATE "PLAIN" with an initial response or a `""` continuation (`"*"` cancels);
  malformed responses are refused without asking the handler; three failed logins end the
  session with BYE. UNAUTHENTICATE, NOOP (with TAG), LOGOUT.
- Script commands only after login, with argument counts, script names (1-255 UTF-8 characters,
  no controls; an empty SETACTIVE name deactivates) and UTF-8 scripts checked first.
- Bounds: 8 KiB per line, 1 MiB per literal, 300 s idle (then BYE), 60 s to finish a started
  command; a grammar error ends the session with BYE "Protocol error".

The handler answers `managesieve_auth` with `managesieve_ok` / `managesieve_no`, and
`managesieve_command` (LISTSCRIPTS, GETSCRIPT, PUTSCRIPT, CHECKSCRIPT, SETACTIVE, DELETESCRIPT,
RENAMESCRIPT, HAVESPACE) with `managesieve_scripts` (LISTSCRIPTS), `managesieve_script`
(GETSCRIPT), `managesieve_ok` (optionally WARNINGS) or `managesieve_no` with a response code.
NetGet does not parse Sieve: validation and storage are the handler's (memory or SQLite), and no
script is ever executed. No answer, or the wrong kind, is NO (TRYLATER).
