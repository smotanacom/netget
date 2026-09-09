# SSH Server Implementation

## Overview

SSH server built on russh, offering an interactive shell and a read-only SFTP subsystem. The
handler — script, static or LLM — decides who may log in, what the shell prints, and what the
SFTP tree contains. Nothing is read from or written to the real filesystem.

**Status**: Experimental
**Port**: 22 (privileged — `privilege_requirement` is `PrivilegedPort(22)`)
**Feature**: `ssh` (`russh`, `russh-keys`, `russh-sftp`, `ssh2` for testing)
**Files**: `mod.rs` (accept loop, russh handler, shell), `sftp_handler.rs` (SFTP), `actions.rs`

### Why Experimental, not Beta

`Beta` means "human reviewed, **works against real clients**", evidenced by a test in which a
third-party implementation completed a real exchange. There is no such test here.

`tests/server/ssh/` does now exist and is substantial — banner, version exchange, concurrent
connections, script-vs-LLM routing, SFTP, and `llm_failure_test.rs` for the fail-closed paths —
but every one of those drives the server through a bare `TcpStream`. The one third-party client
that was tried, `ssh2`, does not complete a session: `test.rs` records that it "has
timing/compatibility issues with russh server". So NetGet is still only checking NetGet.

**russh would not close the gap either.** It is the library this server is built on, so using it
as the peer is the circular case `tests/server/websocket/e2e_test.rs` describes — it would prove
russh round-trips through itself. (The code briefly claimed `Beta` on exactly that reasoning,
while its own `e2e_testing` field said "no automated test exists" three lines below. Both the
rating and the contradiction are gone.)

Raise to `Beta` when a real SSH client — openssh's `ssh`/`sftp`, or a working `ssh2` — completes
auth and a channel exchange in a test that is neither `#[ignore]`d nor skipped when the binary
is absent.

### Fail-closed

`llm_auth_decision` returns `Ok(false)` on every path that is not an explicit grant, and the
three cases stay distinct in the log because they are identical on the wire
(SSH_MSG_USERAUTH_FAILURE):

| Situation | Result | Logged decision |
|---|---|---|
| Handler returns `ssh_auth_decision` with `allowed: true` | accept | `decision=model_accept` |
| Handler returns `ssh_auth_decision` with `allowed: false` | deny | `decision=model_reject` |
| Handler returns no `ssh_auth_decision`, or one without `allowed` | **deny** | `decision=fail_closed_no_answer` |
| LLM call errors or times out | **deny** | `decision=fail_closed_backend_error` + `category=` |

Nothing NetGet writes can produce an accept: `Auth::Accept` is reached only from
`allowed == true`. russh's own `Handler` defaults for `auth_none`, `auth_password`,
`auth_publickey` and `auth_keyboard_interactive` are all `Auth::Reject`, so a method this server
does not override cannot let anyone in either.

One gap worth knowing: `auth_publickey` asks the model with the username alone — the offered
public key is not in the event, so a handler cannot distinguish two keys for the same user.

## Architecture

### Manual accept loop

`russh::server::Server::run_on_address()` was observed to hang without accepting, so the server
binds its own listener (`create_reusable_tcp_listener`, so a restart does not hit `EADDRINUSE`)
and calls `russh::server::run_stream()` per connection, each with its own `SshHandler`. Bind
failure propagates with `?` so `server_startup` reports `Error` rather than a phantom `Running`,
and the accept-loop `JoinHandle` is registered with `AppState::register_server_task()` so
`stop_server` aborts it and releases port 22.

### Host key

An Ed25519 host key is generated at startup and never persisted. Every restart produces a new
identity, so clients print `REMOTE HOST IDENTIFICATION HAS CHANGED`. Acceptable for honeypot and
testing use; there is no option to load a key from disk.

### Shell input handling

Input is echoed and buffered per channel until Enter or a control character:

- printable bytes (0x20–0x7E) are echoed and buffered
- backspace/delete (0x7F, 0x08) echo `\x08 \x08` and pop the buffer
- control bytes echo as `^C`, `^D`, … and are buffered so the handler can see them
- Tab is echoed but not buffered (no completion logic exists)

Output passes through `normalize_line_endings()`, which collapses `\r\n` to `\n` and then
expands every `\n` to `\r\n`, so a handler can emit plain Unix output. After each command the
server writes its own `"$ "` prompt, which is why the action descriptions tell handlers not to
include one.

### Startup parameters

None. `send_first` used to be declared, parsed and discarded — the `ssh_banner` event fires
whenever a shell opens regardless, so the flag never meant anything.

## LLM Integration

Every integration point goes through `call_llm` → `try_execute_event_handler`, so script and
static handlers run in-process at **zero** LLM calls. Only unhandled events reach the model.

### Events

| Event               | When                                        | Parameters |
|---------------------|---------------------------------------------|------------|
| `ssh_auth`          | a login is attempted (may repeat)           | `username`, `auth_type`, `password` |
| `ssh_banner`        | a shell channel opens                       | – |
| `ssh_shell_command` | Enter pressed, or `ssh host <cmd>`          | `command`, `first_input`, `empty_input`, `control` |
| `sftp_operation`    | an SFTP request arrives                     | `operation`, `path`, `handle`, `offset`, `length` |

`auth_type` is exactly `"password"` or `"publickey"`. It previously carried a formatted string
(`"password (user='x', password='y')"`), which broke every handler comparing it against
`"password"` and contradicted its own documented description; the password now travels in its
own `password` field, present only for password logins.

`ssh_shell_command`'s `control` array (`"ctrl_c"`, `"ctrl_d"`, `"ctrl_z"`), `first_input` and
`empty_input` are how a handler branches on special keys. These flags were previously computed
and then used only in a log line, while the protocol prompt claimed the model would "see CTRL_C
in the context flags" — it never did.

### Actions

| Action                | Answers              | Parameters |
|-----------------------|----------------------|------------|
| `ssh_auth_decision`   | `ssh_auth`           | `allowed` (JSON boolean, required) |
| `ssh_send_banner`     | `ssh_banner`         | `banner` |
| `ssh_shell_response`  | `ssh_shell_command`  | `response` |
| `send_ssh_data`       | shell channel        | `data` (lower-level alias of the above) |
| `close_this_connection` | shell channel      | – |
| `wait_for_more`       | shell channel        | – |

Note the parameter names: `ssh_shell_response` takes **`response`**, not `output`, and
`ssh_auth_decision` takes only **`allowed`** — there is no `message` field and no
`close_connection` field. Earlier revisions of this document showed all three; a handler
following them failed with "Missing 'response' parameter". `allowed` must be a real JSON
boolean; `"true"` as a string is now an explicit error rather than a silent denial.

There are no async (user-triggered) actions. `close_ssh_connection` and `list_ssh_connections`
were advertised but each only produced an `ActionResult::Custom` that nothing consumed, reading
a connection map that was never populated — both have been removed.

### SFTP actions

One reply action per request:

| `operation`        | Reply action              | Key fields |
|--------------------|---------------------------|------------|
| `opendir`, `open`  | `sftp_handle`             | `handle` (optional; defaults to the path) |
| `readdir`          | `sftp_directory_listing`  | `entries[]` of `{name, is_dir, size}` |
| `read`             | `sftp_file_content`       | `content` (the **whole** file) |
| `lstat`            | `sftp_file_attributes`    | `size`, `is_dir`, optional `permissions` |
| any                | `sftp_error`              | `code`: `no_such_file` (default), `permission_denied`, `failure`, `op_unsupported`, `eof` |

A client's `fstat` is resolved to the handle's path and arrives as `lstat`. `close` and
`realpath` are answered by the server without consulting a handler; `realpath(".")`, which
OpenSSH sends when a session opens, is mapped to `/`.

`sftp_file_content` returns the entire file and the server applies the request's `offset` and
`length`. It previously returned the full content for every read regardless of offset, so a
client reading in chunks received the file over and over and never reached EOF. Keep the `size`
in `sftp_file_attributes` equal to the byte length of `content` for the same path, or downloads
truncate.

Event data is structured. The old `params` field flattened everything into a string
(`"path='/x', id=3"`) that a script handler had to re-parse.

## Known limitations

- **SFTP is read-only.** Only `init`, `opendir`, `readdir`, `open`, `read`, `close`, `lstat`,
  `fstat` and `realpath` are implemented. `write`, `remove`, `mkdir`, `rmdir`, `rename` and
  `setstat` fall through to `unimplemented()` → `SSH_FX_OP_UNSUPPORTED`. Earlier revisions of
  this document listed all of them as supported; they never were.
- **Binary files cannot be served.** `sftp_file_content` is a JSON string, so file contents are
  its UTF-8 bytes.
- No port forwarding (local/remote/dynamic), no X11 forwarding, no session multiplexing.
- Only `password` and `publickey` authentication; no keyboard-interactive, no certificates.
  For `publickey` the key is validated by russh but is not exposed to the handler, so the
  decision is made on the username alone.
- No readline emulation: no command history, no arrow keys, no tab completion.
- The host key is ephemeral (see above).

## Storage

None. There is no filesystem behind either the shell or SFTP — no file is opened, created or
deleted on the host. Directory trees, file contents and the current working directory are
invented by the handler and kept in its own memory (`set_memory` / `append_memory`).

## Testing

**There is no E2E test.** Verify by hand:

```
ssh -p 2222 -o StrictHostKeyChecking=no admin@localhost
sftp -P 2222 -o StrictHostKeyChecking=no admin@localhost
```

An automated test would need `tests/server/ssh/e2e_test.rs` with mocks covering `ssh_auth`,
`ssh_banner`, `ssh_shell_command` and at least one `sftp_operation` round trip. That directory
is outside this module's ownership.

## Example prompts

### Shell honeypot

```
listen on port 2222 via ssh
Accept user root with password toor; deny everyone else
Banner: "Ubuntu 22.04.3 LTS\nLast login: Mon Jan  1 12:00:00 2024"
uname -a -> "Linux web01 5.15.0-89-generic x86_64 GNU/Linux"
whoami   -> "root"
ls       -> "backup.sql  deploy.sh  notes.txt"
exit     -> close_this_connection
```

### SFTP virtual filesystem

```
listen on port 2222 via ssh
Accept any user with password test
Virtual tree:
  /readme.txt  -> "Hello from NetGet SFTP!\n"  (24 bytes)
  /logs/       -> directory containing access.log and error.log
Answer lstat with sftp_file_attributes, readdir with sftp_directory_listing,
read with sftp_file_content, and anything outside the tree with sftp_error no_such_file
```

## References

- [RFC 4253: SSH Transport Layer Protocol](https://datatracker.ietf.org/doc/html/rfc4253)
- [RFC 4254: SSH Connection Protocol](https://datatracker.ietf.org/doc/html/rfc4254)
- [russh](https://docs.rs/russh/latest/russh/) · [russh-sftp](https://docs.rs/russh-sftp/latest/russh_sftp/)

## Failure behaviour

The three integration points fail in different directions, on purpose.

| Path | On `call_llm` error |
|---|---|
| `ssh_auth` | **Deny.** An unreachable backend is not consent; the client gets a real `SSH_MSG_USERAUTH_FAILURE` rather than a hung authentication. |
| `ssh_banner` | No banner. Cosmetic — the shell still opens and the server writes its own `"$ "` prompt, so nothing waits. Logged on both channels. |
| `ssh_shell_command` (shell) | **Disconnect.** A notice, a non-zero exit status, channel close, and `SSH_MSG_DISCONNECT` with reason 7 (`SSH_DISCONNECT_SERVICE_NOT_AVAILABLE`, RFC 4253 §11.1). |
| `ssh_shell_command` (exec) | **Non-zero exit.** A notice on **stderr**, a non-zero exit status, then eof/close. The session stays up — a one-shot exec owns only its channel. |
| `sftp_operation` | **`SSH_FX_FAILURE`** for every operation. |

### What the peer is told, and what it is not

Never the error. `crate::utils::WireFailure` classifies it and the peer receives one of two
`&'static str` categories; the error itself goes to `tracing` and the status stream. Nothing
derived from it — backend URL, model name, file path, `anyhow` chain — can reach a terminal
someone is logged into, and the `&'static str` return type is what makes that structural rather
than a review habit.

The two categories are kept distinct on the wire so a caller backs off instead of recording a
permanent fault. SSH has no error-code field of its own for this, so the **exit status** carries
it, using the `sysexits.h` values every shell tool already understands:

| Category | Exit status | Text |
|---|---|---|
| `Overloaded` | 75 (`EX_TEMPFAIL`) | `netget: backend at capacity, retry later` |
| `Unavailable` | 69 (`EX_UNAVAILABLE`) | `netget: request could not be processed` |

SFTP v3 has no "busy, try again" status at all — the full set is OK, EOF, NO_SUCH_FILE,
PERMISSION_DENIED, FAILURE, BAD_MESSAGE, NO_CONNECTION, CONNECTION_LOST, OP_UNSUPPORTED — so
both categories map to FAILURE there and the distinction survives only in the log.

### Three outcomes, three `decision=` tags

An explicit refusal, silence, and a backend failure often reach the same code on the wire, so
the log is the only place they can be told apart. Every integration point tags which happened,
the way `src/server/radius/` does:

- `decision=model_accept` / `decision=model_reject` — the handler answered, and said yes or no.
- `decision=fail_closed_no_answer` — the handler ran and produced no reply action.
- `decision=fail_closed_backend_error` — `call_llm` returned `Err`; the tag carries `category=`.

### Fail-open shapes that were removed here

Each of these answered *successfully* when nothing had actually decided anything:

- **exec sent `exit-status 0` on every branch**, including a backend outage, so
  `if ssh host cmd` and `$(ssh host cmd)` could not tell an outage from a command that printed
  nothing. The exit status is the entire answer for a one-shot exec.
- **SFTP `read` answered `SSH_FX_EOF` on backend error.** EOF is a *successful* end of file: at
  offset 0 a client writes out a complete zero-byte file and exits 0, so every download was
  silently truncated to nothing.
- **`opendir` / `open` / `lstat` answered `NO_SUCH_FILE` on backend error.** "The handler is
  unreachable" is a different statement from "this path does not exist", and the second is a
  permanent lie a client may cache. Now FAILURE.
- **A handler that returned zero actions was treated as an answer.** `llm_sftp_operation` falls
  back to `{}`, and every reader then substituted a default — a handle equal to the requested
  path, `0o100644` attributes, empty content — so silence *invented* a directory, a file or a
  stat for any path at all. `sftp_no_answer()` now fails those closed. An empty directory is
  still expressible: it arrives as `sftp_directory_listing` with no `entries`, which is a reply
  with fields, not an empty object.
- **`llm_sftp_operation` rebuilt the error** with `anyhow!("LLM error: {}", e)`, which discards
  the concrete type. `is_overload_error` works by downcasting, so every SFTP failure classified
  as generic and a saturated backend was invisible. The error now propagates unwrapped.

The shell case is the one that changed shape. It used to return "no output, do not close", which
the caller's `if let Ok(..)` accepted and then followed with the usual `"$ "` prompt — so a
backend outage was indistinguishable from a command that ran and printed nothing.
`llm_shell_command` now returns `Err` and the caller tears the session down.

Note libssh2 never shows the in-band notice: `_libssh2_channel_read` drains all pending packets
and returns as soon as one errors, so the disconnect short-circuits the already-queued
CHANNEL_DATA. The bytes are on the wire and OpenSSH prints them. Covered by
`tests/server/ssh/llm_failure_test.rs`, which asserts the auth refusal and, for the shell, the
non-zero exit status and the closed channel.
