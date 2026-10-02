# SSH client evidence

No tests skip or use `#[ignore]`. Run the SSH modules in the `client` target with the `ssh`
feature; also run the existing SSH `server` target modules for neighboring behavior.

| File | Peer | Evidence |
|---|---|---|
| `real_server_test.rs` | Independent OpenSSH sshd | 3 prior command tests; 12 mocked model calls |
| `command_channel_test.rs` | NetGet SSH server | 2 injected-action tests; no model calls |
| `sftp_test.rs` | OpenSSH internal-sftp, NetGet pair, explicit malformed framing fixtures | Read-only SFTP, host authentication, resource and cancellation probes |

`start_sshd` generates ed25519 host/user/stranger keys using `ssh-keygen`, authorizes the user
key and starts unprivileged `sshd -D -e` on loopback. The OS user is the only account an
unprivileged daemon can authenticate. Private-key authentication is real. Missing sshd or
ssh-keygen fails with an installation hint. The config disables PAM/password authentication,
uses StrictModes no for the temporary fixture directory, and declares internal-sftp.
Readiness waits for the listening log, not a fixed sleep. `start_sshd_with_subsystem` can
replace internal-sftp with a deliberately malformed fixture; those probes are not counted as
independent SFTP interoperability evidence. Test-only domain files live in the guard's temp
folder; the production client creates none.

The existing command tests preserve their assertions: model-selected commands write a real
file, including a command built from stdout/stderr/exit status shown to the model; a refused
key never raises ssh_connected; a recursive model response runs exactly five commands before
the four-level follow-up bound. Injected command/unknown-action behavior remains covered.

The SFTP suite checks:

- OpenSSH stat drives a script response into a chunked read of 120000 bytes, directory listing
  reports file name/size, and an injected offset/length reads the selected text window.
  A common set_memory action updates AppState before the follow-up script reads that memory.
  A separately read host-key fingerprint is required, real local/peer addresses are recorded,
  and disconnect appears in the peer log.
- A wrong host pin fails key exchange, unpinned legacy sessions reject SFTP, and path/length,
  offset overflow, NUL, argument types and command length are rejected before wire work.
- Missing files, invalid UTF-8 file contents and a directory exceeding 1024 entries fail;
  LSTAT identifies a symlink and a later read on the same SSH connection succeeds.
- Sixteen parked handlers reserve all available slots. The next operation fails promptly and
  disconnect cancels the intercepts and removes the command handle.
- A valid packet prefix with a stalled body reaches the whole-operation deadline, its channel
  closes, and a command on a new channel succeeds. Removal during a second active subsystem
  request closes the socket and resolves the waiting injection; readiness counts both peer
  subsystem-start logs so the removal actually interrupts active work.
- A NetGet pair serves typed listing/stat/text decisions, including an empty directory batch
  followed by EOF. The fixture reads its generated host key independently using ssh-keyscan;
  production never establishes trust by scanning a key.
- A stalled SSH handshake times out and its accepted TCP socket reaches EOF; an authenticated
  OpenSSH connection reaches its configured idle deadline and closes.
- A command producing more than 1 MiB fails and a later channel remains usable.
- A russh wire fixture floods ignored extended-data type 2 or flow-control messages. The
  transport handler must close SSH within four seconds, below the 30-second operation
  deadline, proving those upstream queues cannot grow independently of the semantic reader.
- Length/count bombs, wrong IDs and trailing bytes fail the bounded codec. An oversized length
  prefix fails before waiting for any body. Large valid NAME batches hit the 2 MiB total reply
  budget below the entry cap; seventeen empty batches hit the no-progress bound.

OpenSSH is independent C code; NetGet's SSH transport is russh. The direct pair verifies the
shared semantic surface but does not replace independent evidence. No pcap/fuzz/second-server
claim is made, and the expanded client remains Experimental. Existing server SFTP evidence
uses independent libssh2/ssh2 and OpenSSH paths; it remains in `tests/server/ssh`.
