# NFS Protocol E2E Tests

## Test Overview

Seven tests in `test.rs`, one in `llm_failure_test.rs` and two in `dos_guard_test.rs`, covering NFSv3 (RFC 1813) over ONC RPC (RFC 5531) on TCP. The server side is
`nfsserve`, which handles RPC/XDR framing and the MOUNT protocol; the LLM answers every filesystem
operation through actions.

## Test Strategy

**Decode the replies.** The suite used to assert only that a TCP connection was accepted. That is why
`NFS_OPERATION_EVENT` could ship with a *comment* where its action list belonged — leaving every
`nfs_*_response` invisible to the model, so no NFS operation could be answered at all — without a
single test failing. Do not add a test whose only assertion is that `TcpStream::connect` succeeded;
it cannot tell an NFS server from any other listener.

**Hand-rolled RPC client.** There is no Rust NFSv3 *server-side* test client, and `nfs3_client`'s
builder cannot be pointed at a port. `test.rs` therefore contains a small ONC RPC/XDR client:

- `RpcClient::call` does TCP record marking (high bit marks the last fragment) and asserts the whole
  reply envelope — REPLY, MSG_ACCEPTED, SUCCESS, an empty AUTH_NULL verifier, and the call's own xid
  echoed back. A server getting any of that wrong is rejected by every real client.
- `Xdr` is a sequential reader whose every accessor asserts it has the bytes it needs, so a truncated
  or misencoded reply fails at the field that is wrong rather than producing a plausible value.
- `read_fattr3` decodes a full NFSv3 `fattr3`.

Only three procedures are needed: `NULL` (both programs), `MOUNTPROC3_MNT`, `NFSPROC3_GETATTR` and
`NFSPROC3_LOOKUP`.

## LLM Call Budget

**Total: 12.** (9 below, 1 startup call in `llm_failure_test.rs` where every NFS operation is
deliberately answered with HTTP 500, and 1 startup call each for the two tests in
`dos_guard_test.rs` — both of which perform *zero* NFS operations against the model, which is
the point of one of them.)

Six lifecycle tests are 1 startup call each and perform no NFS operation.
`test_nfs_mount_and_lookup` is 1 startup + `getattr` (`expect_at_least(1)`) + `lookup`
(`expect_calls(1)`); `getattr` fires three times, once directly and twice more for LOOKUP's post-op
attributes on the object and the directory.

`NULL` on either program reaches no LLM call at all, which is what makes it a clean isolation of the
RPC layer.

## Test Cases

1. **`test_nfs_server_start`** — stack is `NFS`, server is running.
2. **`test_nfs_tcp_connection`** — issues a real `NFSPROC3_NULL` and asserts the void reply, so only
   something speaking ONC RPC for program 100003 can pass.
3. **`test_nfs_multiple_connections`** — three concurrent TCP connections.
4. **`test_nfs_connection_lifecycle`** — connect, close, reconnect.
5. **`test_nfs_port_configuration`** — binds the requested port.
6. **`test_nfs_server_stop`** — asserts the port stops accepting within 2s (retried; the listener
   closes asynchronously). This used to print a warning and pass, the exact failure mode
   `tests/server_stop_releases_port_test.rs` exists to catch.
7. **`test_nfs_mount_and_lookup`** — the real protocol test. Replaces three `#[ignore]`d placeholders
   whose entire bodies were `println!` plus `Ok(())`.

### What `test_nfs_mount_and_lookup` asserts

- `MOUNTPROC3_NULL` answers a well-formed RPC reply.
- `MOUNTPROC3_MNT "/"` returns `MNT3_OK`, a non-empty root file handle, an auth flavor list
  containing AUTH_NULL, and no trailing bytes.
- `NFSPROC3_GETATTR` on that handle returns `NFS3_OK` and a `fattr3` carrying the handler's own
  values: `file_type: "directory"` as `NF3DIR` (2), mode `0o755`, size 4096, uid/gid 1000,
  mtime 1700000000. `nlink` is 1 and `fileid` is 1 — the root's fileid is fixed by the server, not
  taken from the handler. This is the first operation that reaches the model, so it is what proves
  the LLM integration answers at all.
- `NFSPROC3_LOOKUP readme.txt` returns `NFS3_OK`, a file handle distinct from the directory's, and
  post-op attributes for both object (`fileid` 42, the value the handler chose) and directory
  (`fileid` 1), with no trailing bytes.

8. **`test_nfs_answers_serverfault_when_llm_fails`** (`llm_failure_test.rs`) — the LLM-failure
   path. Mocks the startup instruction only, so `nfs_operation` matches no rule and the mock
   answers HTTP 500. GETATTR and LOOKUP must both come back `NFS3ERR_SERVERFAULT` (10006), not
   `NFS3ERR_IO` and emphatically not `NFS3ERR_NOENT` — a client acts on "no such file". It also
   asserts the reply carries no trailing bytes (a malformed reply is just a timeout) and that
   the RPC session survives. `MOUNTPROC3_MNT "/"` reaches no LLM call, so it still succeeds and
   serves as the control.

9. **`test_nfs_refuses_oversize_rpc_record`** (`dos_guard_test.rs`) — a record marker of
   `0xFFFFFFFF` (last-fragment bit plus the maximum 31-bit length) followed by a 40-byte
   `MOUNTPROC3_MNT` whose `dirpath` length is also `0xFFFFFFFF`. `src/server/nfs/guard.rs` must
   answer a 28-byte `GARBAGE_ARGS` accepted reply carrying the attacker's own xid, then close;
   a fresh `RpcClient` must still mount afterwards, which is the control that would catch a
   process-wide abort.
10. **`test_nfs_refuses_a_mount_path_that_would_amplify_into_llm_calls`** (same file) — a
    200-component MOUNT dirpath must answer `MNT3ERR_NOENT` with the `lookup` rule recording
    `expect_calls(0)`. The mock answers every lookup **successfully** on purpose: that is what
    makes the amplification real, so nothing but `MAX_PATH_COMPONENTS` stops the walk.

Both were verified by removing the bound: the first then times out waiting for a reply that
never comes (nfsserve sitting on a 2 GiB buffer), the second records 200 LLM calls and answers
`MNT3_OK`.

The RPC client is shared: `RpcClient`, `Xdr` and `xdr_opaque` are `pub` in `test.rs` and reused
by `llm_failure_test.rs` and `dos_guard_test.rs`. There is one RPC implementation in this
directory, not three.

## Client Library

None for NFS — see above. `tokio::net::TcpStream` carries the hand-rolled RPC.

## Expected Runtime

~1s for the whole suite against the mock harness.

## Not Covered

The framing screen's other bounds (`MAX_FRAGMENTS_PER_RECORD`, `FRAGMENT_BODY_TIMEOUT`,
`MAX_CONCURRENT_CONNECTIONS`) — only the fragment/record size bound is driven from the wire;
READ, WRITE, READDIR, CREATE, REMOVE, SETATTR; model-rejection error paths (`NFS3ERR_NOENT`,
`NFS3ERR_ACCES` from an action carrying `"error"` — the *backend-failure* path is covered);
AUTH_UNIX credentials; multi-fragment requests; UDP transport; script mode.
