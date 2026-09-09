# NFS Protocol Implementation

**Status**: `DevelopmentState::Experimental`.

NFSv3 (RFC 1813) over TCP, default port 2049. 2049 is above 1023, so no privilege is required
and `privilege_requirement` is correctly `None` — a declaration here could never fire.

`nfsserve` v0.10 owns RPC, XDR, message framing and the MOUNT protocol. `LlmNfsFileSystem`
(`src/server/nfs/mod.rs`) implements its `NFSFileSystem` trait, and every trait method turns
into one LLM round-trip.

## Robustness: framing is `nfsserve`'s, and it is not bounded

`nfsserve` owning RPC, XDR and message framing is a real simplification, but it is **not** a
safety guarantee, and the line above reads as though it were.

`nfsserve` 0.10.2 reads a length off the wire and allocates it with no sanity limit in two
places: `rpcwire.rs` resizes its fragment buffer by the 31-bit fragment length, and `xdr.rs`
resizes the `Vec<u8>` that a `dirpath`, a `filename`, a file handle and WRITE data each
deserialise into by a 32-bit length field. A ~40-byte `MOUNTPROC3_MNT` whose `dirpath` length
is `0xFFFFFFFF` therefore asks for a 4 GiB zeroed allocation **before any authentication**,
and Rust aborts the process on allocation failure. A non-last fragment with the EOF bit clear
appends into one buffer indefinitely. There is no read timeout, no connection cap, and a task
is spawned per RPC message.

The MOUNT path compounds it: `nfsserve`'s default `path_to_id` splits the dirpath on `/` and
calls `lookup()` per component, and every `lookup` is one `consult_llm` round-trip. So an
unauthenticated peer can turn one MOUNT call into an unbounded sequential chain of LLM calls,
saturating `--llm-max-concurrent` for every other server in the process.

Neither is fixed. The bounds belong in the framing layer, which means a guard around the
listener or a patched `nfsserve`, and both are the first thing to address before this protocol
is exposed to anything untrusted.

Two related notes on the same layer: because `nfsserve` spawns a task per RPC message, several
`consult_llm` calls can run concurrently for one peer — NetGet's per-connection
Idle→Processing→Accumulating state machine is absent here — and this server calls
`update_connection_stats` nowhere, so the rail shows no peers and no counters for it.

## No storage — and that is the whole design

There is no file table, no directory tree, no attribute cache, no backing store of any kind.
`LlmNfsFileSystem` holds an `OllamaClient`, an `Arc<AppState>`, a `ServerId`, an
`Arc<NfsProtocol>` and a status channel. Nothing else. Every `lookup`, `getattr`, `read`,
`readdir` — all of it comes back from the model.

This is the side of the line a file protocol is supposed to be on, and it is worth contrasting
with `src/server/webdav/`, which serves a real in-process `MemFs` and is marked `Incomplete`
for exactly that reason.

The cost is pushed onto the model: **file IDs must be stable**. If `lookup(dir=1,
"readme.txt")` returns 42 once and 43 the next time, or if `getattr(42)` reports a different
size than the last call, clients cache the difference and misbehave in ways that look like
server bugs. Say so in the instruction.

## The bug that made this protocol unusable

`NFS_OPERATION_EVENT` was declared with

```rust
.with_actions(vec![
    // Include all NFS response actions
    // The LLM will choose the appropriate response based on the operation type
])
```

— a comment where the actions should have been. `call_llm` builds the model's tool list from
`event.event_type.actions`, **not** from `get_sync_actions()`, so the model was offered
`set_memory` / `show_message` / `append_to_log` and nothing else. Every `nfs_*_response` it
produced was rejected as an unknown action, retried twice, and failed. Every operation then
fell through to `error!("No valid nfs_..._response action in LLM response")` and returned
NFS3ERR. **No NFS operation could ever be answered.**

Worse in a dev build: `call_llm` reacts to `EventType::has_no_usable_actions()` with a
`debug_assert!(false, ...)`, so the connection task panicked — silently, while the server
still reported `Running`.

The event now carries `nfs_response_actions()`, the same list as `get_sync_actions()`.

## Events

One event type, `nfs_operation`, for all twelve procedures:

| field | meaning |
|---|---|
| `operation` | `lookup`, `getattr`, `setattr`, `read`, `write`, `create`, `mkdir`, `remove`, `rename`, `readdir`, `symlink`, `readlink` |
| `params` | operation-specific: `fileid`, `dirid`, `filename`, `offset`, `count`, `data`, `mode`, `uid`, `gid`, `start_after`, `max_entries`, ... |

The operation name is the only thing telling the model which response to send, which is why
every response action is advertised on this one event rather than split across several.

## Actions

Ten response actions, one per operation shape. All are advertised on `nfs_operation`.

| action | read back by | key fields |
|---|---|---|
| `nfs_lookup_response` | `lookup` | `fileid`, `error` |
| `nfs_getattr_response` | `getattr` | `file_type`, `mode`, `size`, `uid`, `gid`, `atime`/`mtime`/`ctime` |
| `nfs_setattr_response` | `setattr` | same attribute fields |
| `nfs_read_response` | `read`, `readlink` | `data`, `eof` |
| `nfs_write_response` | `write` | `size`, `mode`, `mtime` |
| `nfs_create_response` | `create`, `create_exclusive`, `symlink` | `fileid`, `size`, `mode` |
| `nfs_mkdir_response` | `mkdir` | `fileid`, `mode` |
| `nfs_remove_response` | `remove` | `success` (**required, and honoured** — see below) |
| `nfs_rename_response` | `rename` | `success` (**required, and honoured** — see below) |
| `nfs_readdir_response` | `readdir` | `entries[{name, fileid, attr?}]`, `eof` |

Any response may carry `"error"` instead; the mapping to NFS status is coarse — the string is
logged, not parsed, and each operation returns its own fixed code (`lookup`/`remove` →
NFS3ERR_NOENT, `read`/`write`/`setattr`/`create`/`mkdir`/`rename` → NFS3ERR_ACCES, `readdir` →
NFS3ERR_NOTDIR, `create_exclusive` → NFS3ERR_EXIST, `readlink` → NFS3ERR_INVAL). Returning
`"error": "Is a directory"` will not produce NFS3ERR_ISDIR.

### `success` on remove and rename

Both actions declare `success` as a required boolean, and for a long time **neither read it**.
Only a non-empty `error` string produced a failure, so `{"type": "nfs_remove_response",
"success": false}` — the obvious way for a model to refuse — was reported to the client as a
completed removal. Same for rename.

Now: `true` performs the operation, `false` is NFS3ERR_ACCES logged `decision=model_reject`,
and an answer that omits the flag is NFS3ERR_SERVERFAULT logged
`decision=fail_closed_no_success` rather than an assumed success. SERVERFAULT for the omission
follows the same reasoning as the table below — a definite code like NFS3ERR_NOENT would tell
the client the file is gone, which it caches and acts on.

### When the model does not answer at all

Those codes are for the model *rejecting* an operation. Two other things can happen, and both
used to be answered as if the model had rejected it:

| situation | status | why |
|---|---|---|
| `call_llm` returned `Err` (backend down, malformed response) | **NFS3ERR_SERVERFAULT** (10006) | RFC 1813's "an error occurred on the server which does not map to any of the legal NFSv3 error values" |
| `call_llm` returned `Err` and `crate::llm::is_overload_error` matches | **NFS3ERR_JUKEBOX** (10008) | "resource temporarily unavailable" — retryable, the NFS equivalent of HTTP's 503 + `Retry-After` |
| the model answered, but with no usable `nfs_*_response` | **NFS3ERR_SERVERFAULT** | silence is not a denial |

`LlmNfsFileSystem::llm_failure` and `llm_no_answer` are the only two places that produce these,
and both log at ERROR on both channels. The point is that a no-answer must never be reported as
a *definite* one: NFS3ERR_NOENT told the client the file does not exist when in truth the server
never managed to ask, and NFS3ERR_IO claimed a hard I/O error on the object. `nfsserve` builds
the RPC reply (MSG_ACCEPTED / SUCCESS, xid echoed) from whatever status is returned, so the
client gets a failed operation rather than an RPC that never completes.
`tests/server/nfs/llm_failure_test.rs` asserts exactly that on the wire.

`mode` is a **decimal** number in JSON: 420 is `0644`, 493 is `0755`.

### Removed

`mount_filesystem` and `unmount_filesystem` were declared as async actions. Both parsed a
`path`, discarded it, and returned `ActionResult::NoAction` — nothing in `nfsserve` or in
`LlmNfsFileSystem` has any notion of an export to mount. Removed.

### Field-name mismatches fixed

Two executors read fields the action definitions never documented, so a model following the
documentation always hit the default:

- `mkdir` read `action["dirid"]` while `nfs_mkdir_response` documents `fileid`. Every
  documented response produced fileid 0, and the client got a directory it could not enter.
- `readdir` read `action["end"]` while `nfs_readdir_response` documents `eof`. Listings were
  always reported complete.

Both now prefer the documented name and still accept the old one.

## Text only — the binary limitation

`nfs_read_response.data` is a `String`, and the `data` field of a write event is a `String`.
Actions must not carry raw bytes or base64 (models cannot reliably produce or parse them), and
no binary path was ever built here.

Concretely:

- **Reads**: `data.as_bytes()` goes on the wire verbatim. Any file whose bytes are not valid
  UTF-8 cannot be served.
- **Writes**: `String::from_utf8_lossy(data)`. A client writing a JPEG hands the model a string
  full of U+FFFD, and the original bytes are gone. If the model then echoes that back on the
  next read, the file is corrupt.

This is a real limitation of the design, not a gap to be filled by adding a hex field — the
project rule forbids one. Serving binary content over NFS is out of scope for this protocol.

## Performance

One model round-trip per NFS procedure. `ls -l` in a directory of ten files is a `readdir`
plus a `getattr` each: eleven calls, tens of seconds. `mount` alone costs several. This is
usable for a honeypot or a demo and unusable for anything else — reach for script handlers
(`event_pattern: "nfs_operation"`) if you need throughput.

## Startup and lifecycle

`NFSTcpListener::bind(...)` runs before the spawn and propagates with `?` and a context, so a
bind failure surfaces as a startup error rather than a server stuck in `Running`. The returned
address uses `get_listen_port()`, so an ephemeral port is reported correctly. The
`handle_forever()` task is registered with `AppState::register_server_task()`, so `stop_server`
releases the socket.

`consult_llm` passes `connection_id: None` — `nfsserve` manages its own connections and does
not expose them, so per-connection state and per-connection scheduled tasks are unavailable
here, and the access log shows no client address.

## Limitations

- NFSv3 only, TCP only. No NFSv2, no NFSv4, no UDP.
- Binary files (above).
- One LLM call per operation (above).
- No per-connection tracking (above).
- `nlink` is always 1, `fsid` always 0, `used` always equals `size`.
- Error strings map coarsely to NFS status codes (above).
- No locking (NLM), no ACLs, no extended attributes.
- Real clients probe aggressively on mount; a model that answers inconsistently will produce
  mount failures whose cause is not obvious from the client's error message.

## Manual verification

```bash
./cargo-isolated.sh run --no-default-features --features nfs --release
# "listen on port 12049 via nfs. Root directory (fileid 1) contains readme.txt (fileid 2,
#  regular file, 14 bytes, content 'Hello from NFS'). Keep file IDs stable."

showmount -e 127.0.0.1                      # exports
# Linux:
sudo mount -t nfs -o vers=3,port=12049,mountport=12049,tcp 127.0.0.1:/ /mnt/netget
# macOS:
sudo mount -o vers=3,port=12049,mountport=12049,tcp,resvport 127.0.0.1:/ /mnt/netget
ls -l /mnt/netget && cat /mnt/netget/readme.txt
```

Expect each command to take seconds, not milliseconds — that is the per-operation LLM call.
Watch `netget.log` at DEBUG to see the operation sequence the client actually issues; it is
longer than you would guess.

## Testing

`tests/server/nfs/test.rs` — connection lifecycle, port configuration, multiple connections,
stop/start, **and a mocked MOUNT + LOOKUP + READ** (`test_nfs_mount_and_lookup`), which
asserts the model'''s answers reach the wire. This section used to say the suite "does not
mount a filesystem" and call for exactly that test; it exists.

`llm_failure_test.rs` asserts SERVERFAULT on the wire when the backend fails, and that the
reply carries no trailing bytes.

## References

- [RFC 1813: NFS Version 3](https://tools.ietf.org/html/rfc1813)
- [RFC 1831: RPC v2](https://tools.ietf.org/html/rfc1831) / [RFC 1832: XDR](https://tools.ietf.org/html/rfc1832)
- [nfsserve](https://docs.rs/nfsserve)
