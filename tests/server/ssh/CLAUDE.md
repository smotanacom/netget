# SSH Protocol E2E Tests

## Test Overview

Tests SSH server implementation with authentication, shell sessions, and SFTP operations. Validates that the LLM can
control SSH authentication decisions, generate shell responses, and provide SFTP filesystem operations. Includes
extensive testing of Python script-based auth handling.

## Two independent clients, and what the second one found

One client can agree with one bug. Until September 2026 every real-client test here drove
`ssh2` (libssh2), and a rating resting on one client rests on that client's leniency —
elsewhere in this repository `etcd`, `grpc` and `mysql` were each Beta on a single client and
each turned out to be unusable by every other conformant implementation.

| file | peer | what only it covers |
|---|---|---|
| `test.rs` | `ssh2` → libssh2 (C) | the SFTP subsystem end to end; script-vs-LLM auth routing |
| `real_client_test.rs` | the real OpenSSH `ssh` binary | algorithm negotiation against OpenSSH 10's own preference list; `exec` with no PTY; the **exit status**; OpenSSH's own rendering of a refusal |

`real_client_test.rs` generates an ed25519 identity with `ssh-keygen`, authenticates with
`BatchMode=yes` (OpenSSH will not read a password without a TTY, which is why a key rather than
a password), runs one `ssh host <command>`, and asserts the exact stdout bytes *and* the exit
code. A second session with a username the model refuses is asserted to be `Permission denied`
and exit 255. **It fails rather than skips** when `ssh`/`ssh-keygen` are absent, and is not
`#[ignore]`d.

Every option it passes exists to make the run reproducible: `-F /dev/null` (ignore the
operator's `~/.ssh/config`, as `psql -X` ignores `~/.psqlrc`), `UserKnownHostsFile=/dev/null`
plus `StrictHostKeyChecking=no` (the host key is regenerated on every start, so it is unknown
by construction), and `IdentitiesOnly=yes` + `IdentityAgent=none` (without them a running
`ssh-agent`'s keys are offered first and **each one is a separate `ssh_auth` event**, which
would make the call budget depend on the developer's agent).

**Verified non-vacuous** by breaking the server twice:

| break | what OpenSSH printed |
|---|---|
| `llm_auth_decision`'s accepted branch returns `Ok(!allowed)` | `netget@127.0.0.1: Permission denied (…)`, exit **255**, on the session the model admitted |
| (the double CLOSE, before it was fixed) | `Disconnecting …: oclose packet referred to nonexistent channel 0`, exit **255**, one run in five — see below |
| `exec_request`'s success exit status forced to `69` | stdout still byte-for-byte correct; **only** the exit code moved, to 69. A test reading stdout alone would have passed |

**What it found (1): `ssh host cmd` failed with exit 255 about one run in five — a real bug,
now fixed.** NetGet sent `SSH_MSG_CHANNEL_CLOSE` twice on an exec channel: once from
`exec_request` with the exit status, once from `channel_eof` when the client's own EOF arrived.
RFC 4254 §5.3 allows one per party, and by then the client has usually freed the channel, so
OpenSSH disconnects with `oclose packet referred to nonexistent channel 0`. It failed *after*
the output and `exit-status 0` had arrived intact, which is exactly why it looked like a flake.
libssh2 ignores the second CLOSE. `SshHandler::close_channel_once` is the fix — 4 failures in
20 runs before, 0 in 25 after — and the test asserts OpenSSH's stderr carries no channel
protocol error, which is the form of the bug that shows on *every* run rather than one in five.

**What it found (2): `ssh host cmd` returns CRLF where a real `sshd` returns LF.**
`normalize_line_endings` runs on the exec path too, where no PTY exists; the CRLF an
interactive session shows comes from the pty's `ONLCR`, not from the server. So
`OUT=$(ssh host cmd)` keeps a trailing `\r`. The libssh2 tests could never see it — they
compare against a string the same test wrote, so `\r\n` on both sides agrees with itself. The
test asserts the current behaviour **and names it as a deviation**, so a fix fails a test that
describes the defect rather than quietly satisfying one that never looked. See
`src/server/ssh/CLAUDE.md` → Known limitations for why the fix is not a one-liner.

## Test Strategy

- **Isolated test servers**: Each test spawns separate NetGet instance with specific instructions
- **Real SSH clients**: `ssh2` (libssh2 bindings) *and* the OpenSSH `ssh` binary
- **Raw TCP for basic tests**: Some tests use raw TCP to verify SSH banner (lower-level validation)
- **Script-focused**: Multiple tests validate script generation and execution for auth
- **SFTP validation**: One comprehensive test for SFTP subsystem operations

## LLM Call Budget

### Non-Scripted Tests (Action-Based)

- `test_ssh_banner()`: 0 LLM calls (just TCP banner check)
- `test_ssh_version_exchange()`: 1 LLM call (handshake setup)
- `test_ssh_connection_attempt()`: 1 LLM call (auth attempt)
- `test_ssh_multiple_connections()`: 3 LLM calls (3 connections × 1 banner each)
- `test_sftp_basic_operations()`: 5-6 LLM calls (auth + 4-5 SFTP operations)

### Scripted Tests (Python Script Generation)

- `test_ssh_python_auth_script()`: 1 LLM call (server setup with script) + 0 for auth events
- `test_ssh_script_update()`: 1-2 LLM calls (setup + optional update) + 0 for auth events
- `test_ssh_script_fallback_to_llm()`: 1 LLM call (setup) + 2 fallback calls (eve, frank)

### Real-Client Test (OpenSSH)

- `openssh_completes_a_session_against_the_ssh_server()`: **4** — 1 startup + 2 auth decisions
  (one per session; OpenSSH offers exactly one key because of `IdentitiesOnly`) + 1 exec
  command. The refused session costs no shell call, because it never opens a channel.

**Total: ~19-22 LLM calls** (exceeds 10 target but acceptable given comprehensive coverage)

**Why Higher Budget?**:

1. SSH protocol complexity requires multiple integration points (auth, shell, SFTP)
2. Script tests validate critical feature (Python script generation) across multiple scenarios
3. Each test validates different aspect (banner, auth, shell, SFTP, script fallback)
4. Could consolidate but would lose test isolation and failure diagnosis clarity

**Optimization Opportunity**: Could consolidate non-scripted tests (banner, version exchange, connection attempt) into
single server. Scripted tests must remain separate to validate different script behaviors.

## Scripting Usage

### Scripted Tests

✅ **Scripting Enabled** for authentication in these tests:

- `test_ssh_python_auth_script()` - Basic script auth (allow alice, deny others)
- `test_ssh_script_update()` - Script update capability (initially deny all, then allow charlie)
- `test_ssh_script_fallback_to_llm()` - Script + LLM fallback (script handles dave, LLM handles eve/frank)

**How Scripting Works**:

1. Prompt asks for script-based authentication
2. LLM returns `open_server` action with `script_inline` parameter (Python code)
3. Server stores script and marks event types as script-handled
4. Future auth events execute script instead of calling LLM
5. Script can return `fallback_to_llm` to delegate to LLM for specific cases

### Non-Scripted Tests

❌ **Scripting Disabled** for these tests:

- `test_ssh_banner()` - TCP-level test (no LLM)
- `test_ssh_version_exchange()` - Protocol-level test
- `test_ssh_connection_attempt()` - Simple auth test
- `test_ssh_multiple_connections()` - Banner test
- `test_sftp_basic_operations()` - SFTP operations test

**Rationale**: These tests focus on specific protocol behaviors and LLM action generation, not script execution.

## Client Library

- **ssh2 v0.9** - Rust bindings to libssh2 (SSH client library)
    - Full SSH protocol support (handshake, auth, channels, SFTP)
    - Blocking API (wrapped in tokio for async tests)
    - Used for authentication and SFTP tests

- **tokio::net::TcpStream** - Raw TCP for banner tests
    - Lower-level validation (SSH version string)
    - Faster than full SSH handshake
    - Used for basic connectivity tests

**Why ssh2?**:

1. Industry-standard SSH client library (libssh2 used by OpenSSH, git, curl)
2. Validates real-world SSH protocol compliance
3. Supports all SSH features (auth, shell, SFTP)
4. Well-tested and reliable

**Why raw TCP for some tests?**:

1. Banner test only needs to read version string (no handshake needed)
2. Faster than full SSH connection (no key exchange overhead)
3. Tests low-level protocol behavior

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~2-3 minutes for full test suite
- Breakdown:
    - Non-scripted tests: ~60-90s (5-6 tests × 10-15s each)
    - Scripted tests: ~60-90s (3 tests × 20-30s each, script generation is slow)
    - SFTP test: ~60-90s (multiple SFTP operations)

**Why Slower Than Other Protocols?**:

1. SSH handshake overhead (key exchange, encryption negotiation)
2. Multiple operations per test (auth + shell/SFTP)
3. Script generation is slow (LLM generates Python code)
4. ssh2 library has blocking API (adds latency)

## Failure Rate

- **Medium** (~10-15%) - SSH protocol complexity and script generation
- Common failures:
    - Script not generated (LLM doesn't include `script_inline`)
    - SSH handshake timeout (russh or ssh2 compatibility issue)
    - SFTP response format (LLM returns wrong JSON structure)
- Rare failures:
    - Authentication always succeeds/fails (LLM ignores instructions)
    - Script syntax error (invalid Python generated by LLM)

## Test Cases

### 1. SSH Banner (`test_ssh_banner`)

- **LLM Calls**: 0
- **Prompt**: Send SSH protocol banner
- **Client**: Raw TCP connection
- **Validation**: Banner starts with "SSH-2.0"
- **Purpose**: Verify SSH server responds with valid version string

### 2. SSH Version Exchange (`test_ssh_version_exchange`)

- **LLM Calls**: 1 (handshake setup)
- **Prompt**: Implement SSH-2.0 protocol, send banner
- **Client**: ssh2 handshake
- **Validation**: Handshake completes (may fail at key exchange)
- **Purpose**: Test SSH protocol negotiation (version exchange phase)

### 3. SSH Connection Attempt (`test_ssh_connection_attempt`)

- **LLM Calls**: 1 (auth attempt)
- **Prompt**: Accept SSH connections, handle version exchange and key exchange
- **Client**: ssh2 handshake + auth attempt
- **Validation**: Handshake completes, auth attempt processed (may succeed or fail)
- **Purpose**: Validate full SSH connection flow up to authentication

### 4. SSH Multiple Connections (`test_ssh_multiple_connections`)

- **LLM Calls**: 3 (3 connections)
- **Prompt**: Handle multiple concurrent SSH connections
- **Client**: 3 sequential TCP connections
- **Validation**: Each connection receives SSH banner
- **Purpose**: Test concurrent connection handling

### 5. SSH Python Auth Script (`test_ssh_python_auth_script`)

- **LLM Calls**: 1 (setup) + 0 (auth via script)
- **Prompt**: Allow user 'alice', deny others, use script
- **Client**: ssh2 auth as alice (should succeed), bob (should fail)
- **Validation**:
    - Output contains "script_inline" (script was generated)
    - Only 1 LLM request total (setup, no LLM for auth events)
    - Alice authenticates successfully
    - Bob authentication denied
- **Purpose**: Validate script generation and execution for authentication

### 6. SSH Script Update (`test_ssh_script_update`)

- **LLM Calls**: 1-2 (setup + optional update) + 0 (auth via script)
- **Prompt**: Initially deny all, then update script to allow 'charlie'
- **Client**: ssh2 auth as charlie (should succeed after update)
- **Validation**:
    - Output contains "script_inline" (initial script)
    - May contain "update_script" (if LLM used update action)
    - At most 2 LLM requests (setup + optional update, no LLM for auth)
    - Charlie authenticates successfully
- **Purpose**: Test script update capability (modifying running server's script)

### 7. SSH Script Fallback to LLM (`test_ssh_script_fallback_to_llm`)

- **LLM Calls**: 1 (setup) + 2 (fallback for eve and frank)
- **Prompt**: Script allows dave, falls back to LLM for others. LLM allows eve, denies frank.
- **Client**: ssh2 auth as dave (script), eve (LLM fallback), frank (LLM fallback)
- **Validation**:
    - Output contains "script_inline" (script was generated)
    - Output may contain "fallback_to_llm" (script returned fallback)
    - At least 1 LLM request, possibly 3 (setup + 2 fallbacks)
    - Dave authenticates (script handled)
    - Eve authenticates (LLM fallback handled)
    - Frank denied (LLM fallback handled)
- **Purpose**: Validate script + LLM hybrid mode (script handles common cases, LLM handles edge cases)

### 8. SFTP Basic Operations (`test_sftp_basic_operations`)

- **LLM Calls**: 5-6 (auth + readdir + open + read + stat)
- **Prompt**: Enable SFTP, virtual filesystem with readme.txt, data.json, logs/
- **Client**: ssh2 SFTP client
- **Operations**:
    1. Authenticate as 'test'
    2. List root directory (/)
    3. Read file (/readme.txt)
    4. Get file attributes (stat /readme.txt)
- **Validation**:
    - Authentication succeeds
    - SFTP channel opens
    - Directory listing returns entries
    - File open succeeds
    - File read returns content
    - File stat returns metadata
- **Purpose**: Comprehensive SFTP subsystem test (auth, directories, files, metadata)

## Known Issues

### 1. SSH Handshake Flakiness

**Symptom**: Occasional handshake failures with "KEX error" or timeout

**Cause**: Compatibility between russh (server) and ssh2/libssh2 (client) on key exchange algorithms

**Workaround**: Tests use `match` with error handling - handshake failures are noted but don't fail test

**Status**: Acceptable for testing - shows protocol is partially implemented

### 2. Script Not Generated

**Symptom**: Test fails with "Server should have been configured with a script"

**Cause**: LLM doesn't generate `script_inline` in response (misunderstands prompt)

**Frequency**: ~10% of script tests

**Workaround**: Retry test or adjust prompt to be more explicit about script requirement

### 3. SFTP Response Format Errors

**Symptom**: SFTP operations fail with parse errors

**Cause**: LLM returns wrong JSON structure for SFTP responses (e.g., missing "entries" field)

**Frequency**: ~5% of SFTP test runs

**Workaround**: Test uses lenient validation (checks for any entries, not specific format)

### 4. Long Runtime for Script Tests

**Symptom**: Script tests take 20-30s each (longer than other tests)

**Cause**: Python script generation is slow (LLM generates code, not just JSON)

**Status**: Expected behavior - script generation is complex

**Optimization**: Could use smaller model for script generation (trades quality for speed)

## Performance Notes

### Why Script Tests Take Longer

Script tests are slower because:

1. LLM generates Python code (more tokens than simple JSON actions)
2. Script execution adds minimal overhead (<1ms)
3. But initial script generation is slow (~5-10s vs 2-5s for action)

However, script tests demonstrate **massive performance improvement** once script is generated:

- Action-based: 1 LLM call per auth attempt (~5s each)
- Script-based: 0 LLM calls per auth attempt (<1ms each)

Example: 100 authentication attempts:

- Action-based: 100 × 5s = 500s (8 minutes)
- Script-based: 1 × 10s (setup) + 100 × 0.001s = 10.1s

### SSH Handshake Overhead

SSH handshake adds ~1-2s overhead per connection:

1. Version exchange (~100ms)
2. Key exchange (~500ms)
3. Encryption negotiation (~100ms)
4. Authentication (~5s with LLM)

This is why `test_ssh_banner` (raw TCP) is faster than `test_ssh_version_exchange` (full SSH).

### SFTP Operation Overhead

Each SFTP operation requires:

1. SFTP packet serialization (~10ms)
2. SSH channel send/receive (~50ms)
3. LLM processing (~5s)
4. SFTP packet deserialization (~10ms)

Total: ~5s per operation (LLM dominates)

## Future Enhancements

### Test Coverage Gaps

1. **Interactive shell**: no test drives a PTY session. `exec` (`ssh host <command>`) *is*
   covered, by `real_client_test.rs`.
2. **SFTP writes**: No tests for file upload or modification
3. **openssh's `sftp` binary**: the SFTP subsystem still rests on libssh2 alone
4. **Multiple channels**: No tests for multiple channels per connection
5. **Connection close**: No tests for LLM-initiated connection close

Item 3 on this list used to read "Public key auth: only password auth tested". That stopped
being true when `real_client_test.rs` landed: OpenSSH authenticates with `publickey` there.

### Consolidation Opportunities

Non-scripted tests could be consolidated:

```rust
let prompt = format!(
    "listen on port {} via ssh.
    Handle multiple concurrent connections.
    For authentication: allow user 'testuser' with password 'testpass'.
    For shell commands: respond with directory listing and command output.
    For SFTP: provide virtual filesystem with readme.txt, data.json, logs/",
    port
);

// Single server handles: banner, version exchange, auth, shell, SFTP
// Would reduce from 5 tests (5 servers) to 1 test (1 server)
// Savings: ~30-40s of test time
```

However, this loses test isolation - one failure affects all validations.

### Script Test Variations

Additional script scenarios to test:

1. **Script error handling**: What if script has syntax error?
2. **Script timeout**: What if script hangs?
3. **Script state persistence**: Can script maintain state across auth attempts?
4. **Multiple event types**: Can one script handle both auth and shell commands?

## References

- [RFC 4253: SSH Transport Layer Protocol](https://datatracker.ietf.org/doc/html/rfc4253)
- [RFC 4254: SSH Connection Protocol](https://datatracker.ietf.org/doc/html/rfc4254)
- [ssh2-rs Documentation](https://docs.rs/ssh2/latest/ssh2/)
- [russh Documentation](https://docs.rs/russh/latest/russh/)
- [SFTP Protocol (draft-ietf-secsh-filexfer)](https://datatracker.ietf.org/doc/html/draft-ietf-secsh-filexfer-02)
