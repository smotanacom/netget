# FTP Server Implementation

## Overview

FTP (RFC 959) **control connection** server. Every reply the client sees is produced by a
handler — script, static or LLM. There is no filesystem behind it: the model invents the
directory listings and file contents, and nothing is read from or written to disk.

**Status**: Experimental
**Port**: 21 (privileged — `privilege_requirement` is `PrivilegedPort(21)`)
**Feature**: `ftp` (no extra crates; `ftp = []` in `Cargo.toml`)
**Files**: `mod.rs` (accept loop, session I/O), `actions.rs` (actions, event, metadata)

## What this is not

Read this before choosing FTP for anything.

**There is no data connection.** `PASV`, `EPSV` and `PORT` are not implemented — the server
never opens a second socket. Everything is written on the control connection. A real FTP
client (`ftp`, `lftp`, `curl`, FileZilla) will therefore:

- happily complete the login handshake (`USER`/`PASS`/`SYST`/`PWD` are plain replies), and
- fail or hang on anything that needs a transfer (`LIST`, `NLST`, `RETR`, `STOR`), because the
  handler's `PASV` reply is whatever the model made up and no data socket is listening behind it.

`send_ftp_list` and `send_ftp_data` write their bytes on the control connection anyway. That is
useful against a raw client (`nc localhost 2121`) and against an attacker who is reading the
transcript, and it is useless for interoperating with a real client. The action descriptions say
so; do not treat listing support as working file transfer.

Also missing: FTPS / `AUTH TLS`, binary vs ASCII mode (`TYPE` is just another reply), `REST`,
and any notion of a current working directory — the model tracks the cwd in its own memory or
not at all.

## Architecture

### Connection flow

1. Accept TCP (`create_reusable_tcp_listener`, so restarts do not hit `EADDRINUSE`).
2. Register the connection in `ServerInstance` so it appears in the TUI.
3. Raise `ftp_command` with `command = "CONNECTION_ESTABLISHED"` and write whatever the handler
   returns — this is how the mandatory 220 greeting is produced.
4. Read command lines with `read_command_line`, raise `ftp_command` per line, write the
   handler's output, repeat.
5. On `close_connection`, EOF, or a write error, mark the connection closed.

`read_command_line` exists because `BufReader::read_line` grows its buffer until it finds a
newline: a peer that connects and streams bytes with no `\n` made the server allocate without
bound, which is a one-connection out-of-memory from an unauthenticated client. It caps the
line at `MAX_COMMAND_LINE` (8 KiB — RFC 959 commands are short) and answers an over-long line
with `500 Command line too long` before closing, rather than dropping the connection silently.

### Dashboard injection (peer handle + counters)

`handle_session` owns `tokio::io::split(stream)`; the write half is an `Arc<Mutex<_>>` shared
with a `peer_support::spawn_peer_command_task`, registered right after the connection is added
and removed on every exit (EOF, both 421 paths, `close_connection`, errors) through the single
return at the end of `handle_session`. So `[ message this peer ]` / `[ disconnect this peer ]`
work: an injected `send_ftp_response` / `send_ftp_multiline` / `send_ftp_data` / `send_ftp_list`
is encoded by the same `execute_action` the handlers use and written to the control connection;
`close_connection` half-closes it. All of FTP's wire actions return `ActionResult::Output`, so
there is no `Custom`-result gap. Every read line and every write goes through
`update_connection_stats`, so the rail's `↓ ↑` counters and `last_activity` are live.
Proven with zero LLM calls in `tests/server/ftp/peer_injection_test.rs`.

The accept-loop `JoinHandle` is registered with `AppState::register_server_task()`, so
`stop_server` aborts it and releases port 21. `spawn_with_llm_actions` propagates bind failure
with `?`, so a port clash is reported as `Error` and not as a phantom `Running` server.

### Startup parameters

None. The previously advertised `passive_port_range` was never read by any code — there is no
passive mode to configure — and has been removed. `send_first` is not declared either: the
server always sends the greeting event itself, so passing `send_first` would only earn an
"unsupported" warning from `server_startup`.

### Error handling

A handler failure is not silent, and the peer never sees the error text. Both paths classify
the failure with `crate::utils::WireFailure` and put only its `&'static str` category on the
wire; the error itself goes to the log:

| Path | Wire | Log |
|---|---|---|
| greeting | `421 Service not available, closing control connection (netget: …)` then close | WARN with the full error |
| command | `421 Service not available, closing control connection (netget: …)` then close | WARN with the full error |

421 rather than silence because an FTP client may not send a command until it has read a
greeting, so a silent greeting failure hangs the client until its own timeout. The two
categories are `netget: backend at capacity, retry later` (overload — retryable) and
`netget: request could not be processed`.

## LLM Integration

`call_llm` is used for both the greeting and every command, so script and static handlers run
in-process with **zero** LLM calls (`call_llm` → `try_execute_event_handler`). Only when no
handler matches does the model get invoked, once per command line.

### Event

| Event         | When                                            | Parameters |
|---------------|-------------------------------------------------|------------|
| `ftp_command` | connection accepted, and once per command line  | `command`  |

`command` is the raw line with the trailing CRLF stripped — not upper-cased, not split into
verb and argument. The single sentinel value `CONNECTION_ESTABLISHED` means "TCP connection
accepted, send your 220 greeting"; it is never sent by a client.

### Actions

| Action                | Sends                                                     | Parameters        |
|-----------------------|-----------------------------------------------------------|-------------------|
| `send_ftp_response`   | `<code> <message>\r\n`                                    | `code`, `message` |
| `send_ftp_multiline`  | `<code>-<line>\r\n` … `<code> <last>\r\n`                 | `code`, `lines[]` |
| `send_ftp_data`       | raw text + CRLF, **on the control connection**            | `data`            |
| `send_ftp_list`       | one CRLF-terminated line per entry, **on the control connection** | `entries[]` |
| `wait_for_more`       | nothing — read another line first                         | –                 |
| `close_connection`    | nothing — closes the control connection                   | –                 |

`code` is validated: it must be a three-digit RFC 959 code in 100–599. Out-of-range values and a
missing `code` or `message` are errors, not silently substituted defaults — a wrong reply code
is worse than a visible failure.

`message`, each element of `lines`, and each `entries` element are rejected if they contain CR
or LF (`reject_line_breaks`). All three descriptions already promised this and nothing checked
it, which left `send_ftp_response` a response-splitting primitive: a `message` of
`"ok\r\n230 Logged in"` makes the client read a second, forged reply and treat a 331 as a
successful login. `send_ftp_data` is deliberately exempt — it is the documented raw escape
hatch for bytes the reply actions cannot express, and it normalises the ending so exactly one
CRLF is written (a bare `\n` is upgraded; previously this path emitted a lone `\r` and
truncated the line for the client).

There are no async (user-triggered) actions.

### Common reply codes

220 ready · 221 goodbye · 230 logged in · 250 command ok · 257 pathname created ·
331 need password · 421 service unavailable · 425 cannot open data connection ·
500 syntax error · 530 not logged in · 550 file unavailable

## Storage

None, by design. The protocol holds no files, no directory tree and no cwd. Directory listings
and file contents come from the handler on every request; `send_ftp_list` formats strings the
model supplies and nothing else.

## Testing

`tests/server/ftp/` has mocked E2E tests (greeting, USER/PASS, PWD/QUIT, the 421 failure path)
and `peer_injection_test.rs` for the dashboard's per-peer injection. To verify by hand:

```
nc localhost 2121
USER anonymous
PASS a@b.c
SYST
PWD
QUIT
```

A real FTP client can be used to exercise login, but not transfers — see "What this is not".

## Example prompt

```
listen on port 2121 via ftp
Reply 220 "NetGet FTP" to CONNECTION_ESTABLISHED
Accept user anonymous with any password (331 then 230)
SYST -> 215 "UNIX Type: L8"
PWD  -> 257 "\"/\" is current directory"
QUIT -> 221 "Goodbye" then close_connection
```
