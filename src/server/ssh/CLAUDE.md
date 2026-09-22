# SSH Server Implementation

## Overview

SSH server built on russh, offering an interactive shell and a read-only SFTP subsystem. The
handler — script, static or LLM — decides who may log in, what the shell prints, and what the
SFTP tree contains. Nothing is read from or written to the real filesystem.

**Status**: Beta
**Port**: 22 (privileged — `privilege_requirement` is `PrivilegedPort(22)`)
**Feature**: `ssh` (`russh`, `russh-keys`, `russh-sftp`, `ssh2` for testing)
**Files**: `mod.rs` (accept loop, russh handler, shell), `sftp_handler.rs` (SFTP), `actions.rs`

### Why Beta

`Beta` means "human reviewed, **works against real clients**", evidenced by a test in which a
third-party implementation completed a real exchange. `test_sftp_basic_operations` is that
test: **libssh2** — reached through the `ssh2` crate, which is a binding to the C library and
shares no line of code with russh — completes the transport handshake, authenticates with a
password, opens the SFTP subsystem, and does `opendir`/`readdir`, `open`+`read` and `lstat`,
with the file's bytes and its declared size asserted exactly. It is not `#[ignore]`d and has no
skip gate. `test_ssh_python_auth_script`, `test_ssh_script_fallback_to_llm` and all three
`llm_failure_test.rs` cases drive libssh2 auth as well, on the granting and the fail-closed
paths.

**russh still would not count**, and that part of the earlier reasoning stands: it is the
library this server is built on, so using it as the peer is the circular case
`tests/server/websocket/e2e_test.rs` describes.

**Two tests in `test.rs` prove nothing on their own, and one of them was quoted as evidence for
years.** `test_ssh_version_exchange` and `test_ssh_connection_attempt` wrap ssh2 in
`match … { Ok => assert, Err => println! }`, so they pass whether or not the client succeeds.
That is the same class as a skip-when-missing gate: a green test that asserts nothing. The
"timing/compatibility issues with russh server" note that held this protocol at Experimental
described *those* two, not `test_sftp_basic_operations`.

**What the earlier demotion got wrong is worth keeping.** This file and `actions.rs` both said
libssh2 "does not complete a session", citing a comment in `tests/server/ssh/test.rs` about
"timing/compatibility issues with russh server". That comment described a *test* bug — libssh2
is blocking, and running it on the Tokio runtime that the in-process mock model also needs
deadlocks the two — which was fixed by moving every libssh2 call onto `spawn_blocking`. The
comment outlived the fix and was quoted twice as the reason for the rating. **Check what a
test drives, not what the server links**: the demotion looked at russh in `src/` and never
noticed `ssh2::Session` in `tests/`.

### Still unproven

- **openssh's own `ssh` and `sftp` binaries** have only ever been driven by hand. They are on
  this machine and in CI, so an automated test is cheap — and they are the client most users
  would point at this server, so until it exists Beta rests on libssh2 alone.
- **An interactive shell through a third-party client.** The libssh2 tests cover exec and
  SFTP; nothing drives a PTY session.

What would settle the shell gap either way: openssh's `ssh`/`sftp`, or `ssh2` again, completing
auth **and a shell/exec channel exchange** in a test that is neither `#[ignore]`d nor skipped
when the binary is absent — the same shape `test_sftp_basic_operations` already has for SFTP.

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

## Connection bounds

Before September 2026 this server accepted without limit, and its only read bound was russh's
`inactivity_timeout`, set to 3600 as an unexplained literal. That is not a bound on the state
that matters: a peer that connected and never sent its identification string held a socket, a
task and an `AppState` row for an hour, before any authentication, on a server that would happily
accept a hundred more. Both halves are declared now; the constants and the reasoning live beside
them in `src/server/ssh/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 60s | Both ends send an identification string as soon as the connection is up (RFC 4253 §4.2). OpenSSH's `LoginGraceTime` for *completing* authentication is 120s; this is half that and applies only to producing a single byte, so it cannot close a peer `sshd` would keep. |
| `IDLE_SESSION_TIMEOUT` | 3600s | **An interactive shell is legitimately silent for a long time**, and SSH has no keepalive that is on by default to sit above — OpenSSH's `ServerAliveInterval` and `ClientAliveInterval` both default to **0**. An hour is the most aggressive bound that is defensible, and it is what this server was already doing; it is now named and passed to *both* russh's config and the stream wrapper, so the two cannot drift apart. |
| `MAX_CONNECTIONS` | 256 | Refusal: **`Exceeded MaxStartups\r\n`**, the exact line OpenSSH's `sshd` writes over `MaxStartups`, and legal SSH — RFC 4253 §4.2 lets a server send lines before its identification string precisely so it can say something to the user, and clients print them. One honest difference: this cap counts every connection, not only unauthenticated ones, so it is not literally MaxStartups; the text is chosen because it is the line SSH users already recognise for "refusing new connections right now". |

**NetGet's own SSH client *speaks inside `connect()`*, so it is never the silent peer this
bound closes:** russh writes the `SSH-2.0-…` identification string and `src/client/ssh/mod.rs`
then runs `authenticate_password` unconditionally, both before the first model turn —
`PROTOCOL_QUALITY.md`'s three-state test.

**Where the deadline lives, and why it is not a `peek`.** russh owns every read once `run_stream`
is called, so there is no `read()` of ours to wrap. The other hyper-backed servers in this sweep
solve that with a `TcpStream::peek` before the crate sees the socket — but that makes the bound
depend on the *client* speaking first, and SSH is the one protocol here where the server
legitimately speaks first. So the socket is wrapped in `DeadlinedStream`, a two-field adapter
over `accept_bounded::IdleTimeoutReader` with the write half passed straight through.

That reader's deadline is armed **lazily**: only while a read is polled and finds nothing, and
disarmed the moment bytes arrive. russh awaits a handler inside the `select!` arm that matched
rather than polling the read alongside it, so during a model round-trip — or a `manual` rule
parked for a human — no clock is running at all, and the next poll arms a fresh deadline. That is
the whole correctness argument, and it is the TFTP live-transfer eviction read in reverse.

`tests/server/ssh/connection_bounds_test.rs` drives both halves from the wire: a peer that never
identifies itself gets the server's `SSH-2.0-…` line and is then closed at the bound, and a peer
that *has* identified itself is still connected 68 seconds later. Handing `run_stream` the raw
`TcpStream` instead of the `DeadlinedStream` makes the first test hang for its whole 100-second
window and fail. `tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is
removed, and `tests/accept_bounded_test.rs` covers the shared helper, including `IdleTimeoutReader`.

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

The suite is `tests/server/ssh/test.rs` (banner, version exchange, concurrent connects,
script-vs-LLM auth routing, and one SFTP round trip) and `tests/server/ssh/llm_failure_test.rs`
(the fail-closed paths: auth, shell command, exec). Nothing in either file is `#[ignore]`d.

`test_sftp_basic_operations` is the one that uses a third-party client end to end: `ssh2`
(libssh2 bindings) handshakes, authenticates with a password, opens the SFTP subsystem and
completes `readdir` / `open` + `read` / `stat`, with unconditional assertions. The other ssh2
tests are lower-level — `test_ssh_version_exchange` notes that ssh2 has timing and
compatibility trouble against this russh server outside that path.

Verify the interactive paths by hand, which no test covers:

```
ssh -p 2222 -o StrictHostKeyChecking=no admin@localhost
sftp -P 2222 -o StrictHostKeyChecking=no admin@localhost
```

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

## Max inbound message size

`MAX_SHELL_LINE_BYTES = 64 * 1024` (`mod.rs`), declared as `metadata().max_inbound_bytes`.

This is the only inbound buffer NetGet owns in this protocol. russh does the transport framing
and bounds a single packet (256 KiB transport, 32 KiB `maximum_packet_size`), but the
per-channel echo buffer in `shell_buffers` is flushed to the model only when a newline or a
control byte arrives — so it accumulates across arbitrarily many packets, one `Vec` per open
channel, and russh replenishes the channel window for free. A peer that opened a shell channel
and sent `'A'` forever grew it without limit. The per-packet bound gives no protection against
cross-packet accumulation; that was verified in the vendored crate rather than assumed.

64 KiB is a policy choice, not a spec number — SSH defines no line limit. It is far above any
command a person types or a script sends, and the buffer becomes an LLM prompt, where anything
past a few kilobytes is cost with no benefit.

**The line is discarded, not the session**, which is the opposite of what the line-oriented text
protocols do. An over-long line on an interactive shell is far more often a paste accident than
an attack, and a newline is a natural resynchronisation point — unlike POP3/NNTP/IMAP, where
the peer is mid-frame and the connection must close. SSH has no size-refusal code, so the
notice goes on the channel (`[netget] input line too long, discarded`) where a user sees it,
and the log carries `decision=fail_closed_oversized_line`.

The check runs before any byte is appended, so the high-water mark is the bound.
