# SSH Client E2E Tests

Two files, declared in `tests/client/ssh/mod.rs`. Nothing is `#[ignore]`d.

| File | Peer | Tests | LLM calls |
|---|---|---|---|
| `real_server_test.rs` | **OpenSSH `sshd`**, run unprivileged | 3 | 12 |
| `command_channel_test.rs` | NetGet's own SSH server | 2 | 0 |

## Running

`--test` names a **target**, not a module path:

```bash
./cargo-isolated.sh test --no-default-features --features ssh \
    --test client -- client::ssh --test-threads=100
```

## `real_server_test.rs` — the evidence the rating rests on

`start_sshd` uses `tests/helpers/real_server.rs` setup commands to make three ed25519 keys with
`ssh-keygen` in the guard's temp dir (host, user, stranger), authorises the user key, and runs
`sshd -D -e -f <dir>/sshd_config` from the resolved absolute path (sshd re-executes itself and
refuses a relative one). The config: `ListenAddress 127.0.0.1`, `Port <probed>`, the temp-dir
`HostKey`, `PidFile` and `AuthorizedKeysFile`, `StrictModes no` (the temp dir's permissions are
the harness's, not a home directory's), `UsePAM no`, password and keyboard-interactive off,
`LogLevel VERBOSE`. Ready when it logs `Server listening on 127.0.0.1 port N`. **It fails, never
skips,** when `sshd` or `ssh-keygen` is missing, naming `openssh-server` / `openssh-client`.

**An unprivileged sshd can log in only the user it runs as**, so the client authenticates as
the current OS user (`getpwuid(getuid())`) with the generated key through the
`private_key_path` startup parameter. That parameter did not exist: the client supported
password authentication only, and an unprivileged sshd cannot check a password. Public-key
authentication is the feature this suite made necessary.

Checked on macOS (Darwin 27) with OpenSSH 10.3p1: unprivileged `sshd` starts, accepts the key, runs
commands and logs one harmless `BSM audit: … Operation not permitted` line per login.

### `ssh_client_runs_the_models_commands_against_openssh` (4 calls)

`ssh_connected` (matched on the username) → a command that `tee`s `hello from the model` into
`out.txt`, writes `a warning` to stderr and exits 3; its `ssh_output_received` (matched on
`exit_code` 3, the stdout and the stderr) → a command appending `the model saw: <stdout> (exit
<code>, stderr: <stderr>)`, all three from the event; that output (`exit_code` 0) →
`disconnect`. Then `out.txt` must hold exactly both lines, and sshd's log must show `Accepted
publickey for <user>` and `Received disconnect from 127.0.0.1`.

### `ssh_client_is_refused_with_an_unauthorised_key_by_openssh` (1 call)

The client connects with the stranger's key. It must print `SSH authentication failed`, the
model must be asked nothing after startup (there is no `ssh_connected` rule, so a connect event
would be an unmatched request), and sshd must not log an accepted key.

### `ssh_client_follow_up_chain_is_bounded_against_openssh` (7 calls)

The model answers every output with `echo tick >> ticks.txt`. The connect event's command runs
at depth 0; the output at `MAX_FOLLOWUP_DEPTH` (4) is shown and its answer dropped, so the file
sshd's shell wrote must hold exactly five ticks.

### What the real server found

- **Every exit status was lost.** The read loop stopped at the channel's EOF, and OpenSSH sends
  `exit-status` after EOF. NetGet's own SSH server never showed it. Verified by mutation:
  stopping at EOF again fails the first and third tests (neither `exit_code` rule matches).
- **Stderr was discarded**; it is now its own field. Verified by mutation.
- **The chain had no bound.** Verified by removing it: the third test fails.

### Why this is condition 4 of the client bar

The file is written by sshd's shell running commands the model chose, one built from the
output it was shown. Verified by mutation: dropping the actions the model returns for an
output makes the first and third tests fail.

## `command_channel_test.rs` (0 calls)

An `execute_command` injected through `AppState::send_to_client` (the dashboard's `[ send ]`)
runs against NetGet's own SSH server, and an unknown action is rejected rather than swallowed.

The five `#[ignore]`d tests that needed an external password-auth SSH server (`e2e_test.rs`:
connect-and-authenticate, execute-command, multiple-commands, auth-failure, disconnect) were
deleted: the real-server suite covers each against an `sshd` it starts itself — with a key, not
a password.

## Not covered

- Password authentication against a real server.
- Host key verification (there is none).
- PTY allocation, SFTP, forwarding, an SSH agent.
