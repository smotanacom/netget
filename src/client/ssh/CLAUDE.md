# SSH Client Implementation

## Overview

SSH client implementation using the `russh` library for connecting to SSH servers and executing commands under LLM
control.

## Library Choices

### Primary Library: russh

**Crate:** `russh` v0.45
**Why:** Pure Rust SSH implementation with async support and good channel management.

**Key Features:**

- Full SSH protocol implementation (SSH-2)
- Password and public key authentication
- Channel multiplexing
- Command execution via channels
- Active maintenance and good documentation

**Alternatives Considered:**

- `ssh2` - Bindings to libssh2 (C library), less idiomatic Rust
- `thrussh` - Older name for russh (renamed)

### Key Library: russh-keys

**Crate:** `russh-keys` v0.45
**Why:** Key management and cryptography for russh.

**Features:**

- Loading the private key for public-key authentication (`load_secret_key`, OpenSSH format,
  optionally passphrase-protected)

## Architecture

### Connection Model

**Connection Lifecycle:**

1. Resolve hostname and connect to SSH server
2. Perform SSH handshake and version exchange
3. Authenticate (password, or public key from `private_key_path`)
4. Trigger `ssh_connected` event to LLM
5. Wait for LLM to issue commands
6. Execute commands via SSH channels
7. Return output to LLM via `ssh_output_received` event
8. LLM can chain commands or disconnect

**Channel Management:**

- Each command execution opens a new SSH channel
- Channels are closed after command completion
- No persistent shell session (command mode only)

### State Machine

**Client State:** Idle → Processing → Accumulating (standard pattern)

**Note:** For SSH, the state machine is simplified because:

- Commands are discrete (one command = one channel = one LLM call)
- No streaming data accumulation like TCP
- Each command waits for completion before next

### Authentication

Startup parameters: `username` (required), `auth_method` (`password` / `publickey`),
`password`, `private_key_path`, `private_key_passphrase`.

- **`publickey`** — `private_key_path` is loaded with `russh_keys::load_secret_key` before the
  TCP connection is opened, so an unreadable key is reported as that rather than as a failed
  handshake. The key's contents never reach the model, an event or the log. When
  `private_key_path` is given and `auth_method` is not, public-key auth is used.
- **`password`** — the default when no key path is given; `password` is then required.
- A refused credential fails the connect with `SSH authentication failed: the server refused
  the <method> credential for '<user>'`, and no `ssh_connected` event is raised.
- **The server's host key is accepted unconditionally** — there is no `known_hosts` check and
  no pinning parameter, so the client is not safe against an active network attacker.

Not implemented: SSH agent, keyboard-interactive, certificates.

## LLM Integration

### Event Flow

1. **Connection Event:** `ssh_connected`
    - Triggered after successful authentication
    - Provides remote_addr and username
    - LLM can issue initial commands

2. **Output Event:** `ssh_output_received`
    - Triggered after command execution completes
    - Provides `command`, `output` (stdout), `stderr` (when the command wrote any) and
      `exit_code` (when the server sent one)
    - LLM analyzes output and decides next action

**Reading a command's result waits for the channel to close, not for EOF.** EOF only says no
more data is coming; OpenSSH sends `exit-status` *after* its EOF and then closes the channel.
The loop stops at CLOSE, at the end of the channel, or at EOF once the status is in hand. It
used to stop at EOF, and against a real `sshd` every command's exit status was lost —
`tests/client/ssh/real_server_test.rs` matches on `exit_code`, and putting the early break back
fails it. Stderr (extended data type 1) is collected separately.

**The follow-up chain is bounded.** An output's answer is executed and its own output comes
back in turn; `MAX_FOLLOWUP_DEPTH` (4) stops a model that answers every output with another
command. The output at the bound is shown to the model and its answer dropped with a warning.
The real-server test counts the commands sshd ran (exactly five), and fails without the
bound.

### Actions

**Async Actions (User-Triggered):**

- `execute_command` - Run a shell command
- `disconnect` - Close SSH connection

**Sync Actions (Response to Output):**

- `execute_command` - Run follow-up command
- `wait_for_more` - (no-op for SSH, included for consistency)

### Action Examples

```json
{
  "type": "execute_command",
  "command": "ls -la /home"
}

{
  "type": "execute_command",
  "command": "cat /etc/hostname"
}

{
  "type": "disconnect"
}
```

## Data Flow

```
User Instruction → Connect to SSH server → Authenticate
                                               ↓
                                    ssh_connected event
                                               ↓
                                    LLM receives event
                                               ↓
                                    LLM returns action: execute_command
                                               ↓
                                    Open channel, execute command
                                               ↓
                                    Read output, close channel
                                               ↓
                                    ssh_output_received event
                                               ↓
                                    LLM analyzes output
                                               ↓
                            LLM decides: execute_command (next) or disconnect
```

## Logging

**Dual Logging Pattern:**

- All logs via tracing macros (`info!`, `debug!`, `trace!`, `error!`)
- Important events via `status_tx.send()` for TUI

**Log Levels:**

- `INFO` - Connection, authentication, command execution start
- `DEBUG` - Channel operations, exit codes
- `TRACE` - Command output, data transfer details
- `ERROR` - Authentication failures, connection errors

## Limitations

### Current Limitations

1. **Authentication:**
    - Password and public key; no SSH agent, keyboard-interactive or certificates
    - Server key verification disabled

2. **Command Execution:**
    - Command mode only (no interactive shell)
    - No pseudo-terminal (PTY) allocation
    - No stdin streaming to running commands

3. **File Transfer:**
    - No SFTP support yet
    - No SCP support yet

4. **Advanced Features:**
    - No port forwarding
    - No X11 forwarding
    - No SSH tunneling

### Security Considerations

**⚠️ IMPORTANT:** This implementation is for testing and development only.

**Security Issues:**

- Accepts all server keys (MITM risk)
- No host key verification

**For Production Use:**

- Implement proper host key verification (`~/.ssh/known_hosts`)
- Prefer public key authentication
- Add certificate validation
- Enable SSH agent support

## Performance

**Resource Usage:**

- Lightweight (russh is pure Rust)
- One channel per command (channels are cheap)
- No persistent processes

**Latency:**

- SSH handshake: ~100-500ms
- Authentication: ~50-200ms
- Command execution: depends on command
- LLM call: ~500-2000ms (dominant factor)

## Error Handling

**Connection Errors:**

- DNS resolution failures
- Network timeouts
- SSH handshake failures

**Authentication Errors:**

- Invalid credentials
- Unsupported auth methods
- Server rejection

**Command Errors:**

- Channel open failure
- Command execution timeout
- Non-zero exit codes (reported to LLM)

## Future Enhancements

### Phase 1 (Current)

- ✅ Password authentication
- ✅ Public key authentication (`private_key_path`)
- ✅ Command execution
- ✅ Output capture (stdout, stderr, exit status)

### Phase 2 (Next)

- [ ] Host key verification
- [ ] PTY allocation for interactive commands

### Phase 3 (Advanced)

- [ ] SFTP file transfer
- [ ] SCP file transfer
- [ ] Port forwarding
- [ ] Interactive shell session

### Phase 4 (Expert)

- [ ] SSH agent integration
- [ ] Certificate authentication
- [ ] Jump host (ProxyJump) support

## Testing Strategy

See `tests/client/ssh/CLAUDE.md` for detailed testing approach.

The evidence is `tests/client/ssh/real_server_test.rs`: OpenSSH's `sshd` run unprivileged per
test from a config in a temp dir, public-key auth as the current user.

## Example Prompts

```
"Connect to SSH at localhost:22 with user 'admin' and password 'test', then execute 'uname -a'"

"SSH to 192.168.1.100:22 as root, check disk usage with 'df -h', then list processes"

"Connect via SSH to server.example.com as deploy user, execute 'git pull' in /var/www/app"
```

## References

- [russh documentation](https://docs.rs/russh/)
- [russh-keys documentation](https://docs.rs/russh-keys/)
- [SSH Protocol RFC 4253](https://datatracker.ietf.org/doc/html/rfc4253)
- [OpenSSH manual](https://www.openssh.com/manual.html)

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into the running client. `command_loop` in
`mod.rs` drains the bounded channel and runs each action through `apply_ssh_result` — the
same function the `ssh_connected` and `ssh_output_received` LLM paths use, so an injected
`execute_command` opens a channel, execs, reads the output and raises
`ssh_output_received` exactly like an LLM-produced one.

The channel is registered **before** the `ssh_connected` LLM call, which a `*` -> manual
routing rule can park for minutes.

Outcomes:

| action | `ClientSendOutcome` |
|---|---|
| `execute_command` | `Executed { detail: "execute_command \"…\": exit_code=N, K bytes of output" }` |
| `disconnect` | `Disconnected` (handle dropped) |
| `wait_for_more` | `Executed { detail: "wait_for_more" }` |
| unknown verb | `Rejected { error }` |

**SSH never reports `Sent`.** russh owns the encrypted transport, so NetGet never sees a
wire byte count; claiming one would be a lie.

Two locking rules this made load-bearing, because the command loop and the LLM path now share
the same `Arc<Mutex<Handle<..>>>` and `Arc<Mutex<ClientData>>`:

- The session guard is dropped as soon as `channel_open_session()` returns. Holding it across
  the exec plus the follow-up LLM call self-deadlocked the recursive follow-up action (it locks
  the same mutex) and would block every injected command for the whole round-trip.
- The memory string is cloned out of `ClientData` before each `call_llm_for_client`, never
  borrowed across it.

## Maturity: Beta

Rated against the four-condition client bar in the root `CLAUDE.md`, on the evidence in
`tests/client/ssh/real_server_test.rs` (see `tests/client/ssh/CLAUDE.md`):

1. **Real third-party server** — OpenSSH's `sshd` (C), run unprivileged per test with host and
   user keys made by `ssh-keygen`. NetGet's side is `russh`, which shares no code with OpenSSH.
   (NetGet's own SSH *server* is also russh; it is not involved.)
2. **Fails rather than skips** — a missing `sshd` or `ssh-keygen` is a test failure naming the
   brew formula and the Ubuntu package; nothing is `#[ignore]`d. CI's `registry-audit` installs
   `openssh-server` and runs the suite in its evidence loop.
3. **A real session** — key exchange, public-key authentication as the current user, a session
   channel per command with stdout, stderr and exit status read back, and a disconnect; plus a
   refused key.
4. **Acts on the model's answer, asserted on the wire** — the commands the model chose ran on
   the server and wrote a file, one line of it built from the stdout, stderr and exit status
   the model was shown; sshd's own log records the login and the model's disconnect. Verified
   by mutation: dropping the actions the model returns for an output makes the test fail.

Not covered by that evidence: password authentication against a real server (an unprivileged
sshd cannot check a password), host key verification (there is none), PTY, SFTP, forwarding.
