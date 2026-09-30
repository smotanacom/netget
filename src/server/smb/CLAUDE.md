# SMB Protocol Implementation

## Overview

SMB2 (Server Message Block version 2) file server implementing a subset of MS-SMB2. Real
clients — Samba's `smbclient` and the Python `smbprotocol` library — complete whole sessions
against it; the LLM controls the virtual filesystem, authentication, and file operations.

**Protocol**: SMB 2.1 (dialect 0x0210), or 2.0.2 (0x0202) if that is all the client offers.
**Transport**: Direct TCP (MS-SMB2 2.1): every message, or compound chain of messages, is
preceded by a zero byte and a 24-bit big-endian length, and every reply is framed the same
way. That frame is the only thing that says where a request ends — the SMB2 header carries no
length — so the server reads each request whole before deciding anything about it. An RFC 1002
session request (a client that dialled 139) gets a positive session response; keep-alives are
ignored; anything else as the first byte closes the connection.
**Port**: 445 (standard), configurable
**Status**: Beta (see "Maturity" at the end)
**Startup parameters**: the three read deadlines and nothing else — `first_byte_timeout_secs`
(default 30), `idle_timeout_secs` (900) and `body_timeout_secs` (30), each defaulting to the
constant the server uses; zero is refused at startup. See "Connection bounds".

**Code layout**: `wire.rs` owns every byte — the transport frame, the compound walker
(`next_in_chain`), one parser per request (`parse_negotiate` … `parse_query_directory`), the
one function that lays out a response header (`ResponseHeader::encode`), every response body
and every information class — as pure functions, so the whole of the peer-controlled parsing
is fuzzed without a socket (`fuzz/fuzz_targets/smb2_request.rs`). `auth.rs` owns the
SPNEGO/NTLMSSP tokens (`fuzz/fuzz_targets/ntlmssp_token.rs`). `mod.rs` owns the connection,
the per-connection state, the bounds and the model.

## Library Choices

- **Manual SMB2 implementation** - No library used. The `smb` feature still pulls in the
  `smb-msg` crate, which no code references (`grep -rn smb_msg src/` is empty); removing it
  is a `Cargo.toml` and `Cargo.lock` change left for a pass that owns those files
    - SMB2 binary protocol parsing and response generation
    - Custom packet builders for Negotiate, Session Setup, Tree Connect, etc.
    - Direct control over all protocol aspects
- **tokio::net::TcpListener** - TCP connection management

**Why manual implementation?**

- No suitable Rust SMB2 server library exists
- Full control needed for LLM integration at protocol level
- SMB2 protocol is complex but manageable for core operations
- Allows honeypot behavior (accept invalid requests, log probes)

## Architecture Decisions

### Simplified SMB2 Dialect

Implements an SMB 2.0.2/2.1 subset:

- **Negotiate** - picks 0x0210 if offered, else 0x0202, else STATUS_NOT_SUPPORTED. Offers
  SPNEGO with NTLMSSP as its only mechanism, signing enabled but not required, no
  capabilities (no DFS, leasing or multi-credit). `MaxWriteSize`, `MaxReadSize` and
  `MaxTransactSize` are `MAX_WRITE_SIZE`, `MAX_READ_SIZE` and `MAX_TRANSACT_SIZE` (1 MiB
  each), and each is enforced — see "Inbound size bounds".
- **Session Setup** - see "Authentication" below.
- **Tree Connect** - parses the UNC path and allocates a tree id bound to the session. Every
  share name is accepted with **no LLM call**: the share is only a name for the root of the
  tree the model invents, and admission was decided at SESSION_SETUP. `IPC$` connects as a
  pipe share and every CREATE on it is refused (no named pipes).
- **Create** - open/create files and directories; the model decides (below). An open
  carrying `FILE_DELETE_ON_CLOSE` is how SMB2 deletes (smbclient's `rm`), and the event says
  so with `delete_on_close: true`.
- **Read/Write** - file content operations.
- **Close** - close file handles; an unknown handle is STATUS_FILE_CLOSED. No model call.
- **Query Info** - file and file-system information classes.
- **Query Directory** - directory listings, across as many calls as the buffer needs.
- **Logoff, Tree Disconnect, Echo, Flush** - answered, with no model call (Flush checks the
  handle; nothing is buffered). **Cancel** gets no answer (MS-SMB2 3.3.5.16). **IOCTL** is
  STATUS_INVALID_DEVICE_REQUEST (no FSCTL is implemented). LOCK, SET_INFO, CHANGE_NOTIFY,
  OPLOCK_BREAK and anything unknown are STATUS_NOT_SUPPORTED.
- **Compound requests** - split on `NextCommand`; `RELATED_OPERATIONS` inherits the session,
  tree and "the handle just opened" (FileId all ones); replies chained with 8-byte alignment.
  At most `MAX_COMPOUND_REQUESTS` (32) are acted on per frame and one compound reply carries at
  most `MAX_RESPONSE_FRAME_BYTES` — see "Connection bounds".

**Not implemented**:

- SMB 3.x features (encryption, multichannel, secure negotiate, etc.)
- Signing (no session key exists to sign with)
- Opportunistic locks, leases, durable handles
- SET_INFO (so no rename, truncate or set-times; delete works only through
  `FILE_DELETE_ON_CLOSE`, which is what smbclient uses and smbprotocol's `remove()` does not),
  LOCK, CHANGE_NOTIFY, DFS, named pipes
- SMB1: an `\xFFSMB` negotiate is logged and the connection closed

### LLM-Controlled Filesystem

Similar to NFS, LLM controls entire filesystem:

- **Authentication** - LLM decides who can connect
- **File operations** - LLM provides file content, attributes
- **Directory structure** - LLM defines folders and files

### Authentication

A real client sends two SESSION_SETUPs (`auth.rs`):

1. SPNEGO `negTokenInit` wrapping an NTLMSSP NEGOTIATE. The server allocates a session id and
   answers an NTLMSSP CHALLENGE with `STATUS_MORE_PROCESSING_REQUIRED`, **without asking the
   model** — nothing has been said about who is logging in yet. Because nobody is asked, the
   sessions this opens are bounded per connection (`MAX_SESSIONS_PER_CONNECTION`, 16, counted
   with admitted sessions); a NEGOTIATE leg naming a session whose exchange is still in
   progress restarts that exchange instead of opening another (MS-SMB2 3.3.5.5). A malformed
   AUTHENTICATE, a refused login and a failed model call all forget the session.
2. SPNEGO `negTokenResp` wrapping an NTLMSSP AUTHENTICATE naming the user. This is what the
   model decides on: `smb_operation` with `operation: "session_setup"`, `username`, `domain`,
   and `auth_type` — `"anonymous"` (empty user, empty NT response) or `"ntlm"`, which also
   carries `password_verified: false`.

A bare NTLMSSP token (no SPNEGO, as smbprotocol sends with `auth_protocol="ntlm"`) is answered
bare. A SESSION_SETUP with an **empty** security buffer is a one-step guest login decided the
same way (`auth_type: "guest"`), which is what the raw-packet tests use. A security buffer
with no NTLMSSP token at all (Kerberos only) is refused STATUS_LOGON_FAILURE,
`decision=fail_closed_unsupported_mechanism`.

**SPNEGO is not parsed as ASN.1.** `auth::find_ntlmssp` scans the security buffer for the
NTLMSSP signature rather than walking the DER, because every NTLMSSP field is addressed from
that signature and nothing the server needs is in the wrapper. So there is no recursion and no
depth to bound; the `ntlmssp_token` fuzz corpus carries a 20 000-level nested-DER bomb for the
day a real DER walker replaces the scan.

**This authenticates nothing.** No password is checked and no session key is derived, so
every granted session is flagged `IS_GUEST` — or `IS_NULL` for an anonymous login — which is
also what tells a client there is nothing to sign with. The NTLMSSP exchange exists because
no real client finishes SESSION_SETUP without it; `auth.rs` says so in its first paragraph.

### File Handle Management

Server maintains file handle state:

- 16-byte FileId per open, derived from a per-connection counter; at most
  `MAX_OPEN_FILES_PER_CONNECTION` (1024) open at once
- HashMap of handles → path, tree id, attributes, the size once the model has given it, and
  any directory enumeration in progress
- A handle is only found through the tree it was opened on

### Binary Protocol Handling

Manual SMB2 packet parsing (`wire.rs`):

- `RequestHeader::parse` reads the 64-byte header; `ResponseHeader::for_request` echoes the
  request's MessageId, CreditCharge, TreeId and SessionId, carries `RELATED_OPERATIONS`,
  sets `SERVER_TO_REDIR` and grants credits (the request's `CreditRequest`, 1..=64)
- `ResponseHeader::encode` is **the only code that lays a header out**; every body builder
  takes a `ResponseHeader`. The offsets are named constants beside it (MS-SMB2 2.2.1.2)
- Every body builder is a pure function; `tests/server/smb/header_layout_test.rs` calls each

## Connection Management

### TCP Connection Lifecycle

1. Client connects to TCP port
2. SMB2 Negotiate exchange
3. Session Setup (authentication)
4. Tree Connect (share connection)
5. File operations (Create, Read, Write, Close)
6. Disconnect

### Connection Tracking

Connections tracked in ServerInstance state:

- Connection ID per TCP connection
- Protocol-specific info: **none**. The connection is registered with
  `ProtocolConnectionInfo::empty()` and never updated.
- Stats: bytes_sent, bytes_received, packets_sent, packets_received. One message is read in
  two reads — the 4-byte transport header, then the frame it announces — and `SmbReader`
  counts both, flushed after the header and again after the frame, so a 64 KiB WRITE shows as 64 KiB received.
- Status updated on connection close

### Dashboard injection (peer handle)

Every connection registers a peer handle (`server::peer_support`) **before its first read**.
SMB2 is client-speaks-first, so a manual `*` rule parks the very first NEGOTIATE for a human —
the operator has to be able to reach, or hang up, a peer that has said nothing yet. The socket
is split with `tokio::io::split` and the write half shared as an `Arc<Mutex<WriteHalf>>` between
the session and the peer-command task, so an injected write cannot interleave with a response
half-way through a frame. The handle is removed on every exit path, including the error ones.

**What an injected action can and cannot do here, honestly:**

| Action | Effect |
|---|---|
| `close_connection` | Half-closes the write half. This is `[ disconnect this peer ]`, and it works. |
| any wire verb (`smb_read_file`, `smb_list_directory`, …) | Executes and **writes nothing**. The outcome is `ClientSendOutcome::Executed`, not `Sent`. |

The second row is a property of SMB2, not a gap in the plumbing: every response echoes the
request's MessageId, TreeId and SessionId, so a response cannot be encoded without a request to
correlate with. `SmbProtocol::execute_action` therefore returns an `ActionResult::Custom` for
every wire verb and the server's own loop is what turns it into a frame against the request in
hand — `grep -c 'ActionResult::Output' src/server/smb/actions.rs` is 0. Injected from outside a
request there is nothing to correlate with. `[ message this peer ]` on an SMB peer is
consequently of limited use; `[ disconnect this peer ]` is the affordance that matters, which is
exactly the case where a parked request leaves an operator deciding about a connection they
would otherwise have no way to reach.

`close_connection` is the one action `execute_action` resolves itself, and it is deliberately
**not** advertised in `get_sync_actions()` or on `smb_operation` — adding it there would change
the model's tool list. The dashboard injects a bare `{"type": "close_connection"}` whatever a
protocol calls its own close verb, so the arm has to exist for the button to do anything.

`tests/server/smb/peer_inject_test.rs` pins both rows, and that the session's own exit path
releases the handle, with zero LLM calls.

### Per-Connection State

`SmbConnectionState` maintains:

- **Sessions**: HashMap<session_id, SmbSession> — a session exists from the first NTLMSSP leg
  and is `authenticated` only once the model admitted it; at most 16
- **Trees**: HashMap<tree_id, SmbTreeConnect> — bound to the session that connected it; at
  most 64, and TREE_DISCONNECT and LOGOFF return slots
- **Files**: HashMap<file_id, SmbFileHandle>; at most 1024, and CLOSE, TREE_DISCONNECT and
  LOGOFF return slots
- Next session, tree and file-index counters

### Concurrency

Multiple concurrent connections supported:

- Each connection handled in separate tokio task
- Connection state isolated (no shared mutable state)
- LLM calls serialized per operation

## State Management

### Server State

Minimal global state:

- Server ID for LLM context
- Connection tracking for UI

### Connection State

Per-connection state in a `Mutex<SmbConnectionState>` owned by the connection's task:

- Sessions: Maps session_id → username, authenticated flag
- Trees: Maps tree_id → session, share name, pipe or disk
- Files: Maps file_id → path, tree, attributes, size, pending directory entries

### Filesystem State

LLM maintains filesystem via instructions:

- File paths stored in file handles
- LLM consulted for file content on demand
- No persistent storage

## Limitations

### Simplified SMB2 Implementation

- **SMB 2.0.2 / 2.1 only** - No SMB 3.x features
- **Guest and anonymous sessions only** - NTLMSSP is walked, never verified; no Kerberos
- **No encryption, no signing** - there is no session key
- **No oplocks or leases**

### Protocol Simplifications

- Timestamps are the model's `modified_time` where it gives one (directory entries,
  `smb_get_file_info`), otherwise zero
- File attributes are NORMAL or DIRECTORY
- The volume (FileFsSize/FullSize/Volume/Attribute/Device/SectorSize information) is a fixed
  report — 1 GiB, half free, 4 KiB clusters, "NTFS" — because there is no disk to measure

### Inbound size bounds

The Direct TCP header announces a message's length before any of it is read, so the frame
length is the one peer-chosen size this server allocates for. `MAX_MESSAGE_BYTES`
(`MAX_WRITE_SIZE` + 64 KiB) bounds it and is the declared `max_inbound_bytes`. A frame
announcing more is refused after reading only its 64-byte SMB2 header — enough to answer
`STATUS_INVALID_PARAMETER` to the right MessageId — logged
`decision=fail_closed_message_too_large`, and the connection closes, because the rest of the
frame is unread and reading on would parse it as the next message.

Inside a legal frame, a WRITE whose `Length` exceeds `MAX_WRITE_SIZE` (1 MiB, advertised as
`MaxWriteSize` from the same constant) is refused `STATUS_INVALID_PARAMETER` before the model
— MS-SMB2 3.3.5.13 — logged `decision=fail_closed_write_too_large`. The frame was read whole,
so the connection stays in step. A `Length` that runs past the end of the frame is
`STATUS_INVALID_PARAMETER` too. `tests/server/smb/inbound_limit_test.rs` drives both bounds
from the wire, and both were verified by removal.

The other two negotiated maxima are enforced the same way, before the model: a READ longer
than `MaxReadSize` (MS-SMB2 3.3.5.12, `decision=fail_closed_read_too_large`) and a QUERY_INFO or
QUERY_DIRECTORY whose `OutputBufferLength` exceeds `MaxTransactSize` (3.3.5.18, 3.3.5.20,
`decision=fail_closed_transact_too_large`) are `STATUS_INVALID_PARAMETER`.
`bounds_test.rs::read_and_output_buffer_lengths_past_the_negotiated_maxima_are_refused` drives
each at the bound and one past it; each check was verified by removal.

Because every request is read whole, a refused request never leaves bytes in the stream: a
WRITE that arrives before any session is refused `STATUS_USER_SESSION_DELETED` with its payload
consumed, and the next message on the connection is parsed as a message
(`header_layout_test.rs::a_write_before_any_session_is_refused_and_the_stream_stays_in_step`).

### LLM Performance

- **CRITICAL**: Every file operation calls LLM (slow)
- High latency (seconds per operation)
- Not suitable for real file sharing workloads
- Script and static handlers do work: `call_llm` dispatches them, so an
  `event_handlers` entry on `smb_operation` answers without a model round-trip

### Testing Limitations

- Two real clients complete sessions (`tests/server/smb/real_client_test.rs`): Samba's
  `smbclient` and Python's `smbprotocol`, between them every verb that succeeds here (see
  "Maturity"). Windows Explorer, macOS `mount_smbfs` and Linux `mount.cifs` have not been run
  against it; the kernel clients in particular negotiate SMB 3.x first and may refuse a server
  that offers only 2.x
- Some clients probe with an SMB1 negotiate (not supported: the connection closes)
- The blocking-socket raw-packet tests use `#[tokio::test(flavor = "multi_thread")]`. They must: the mocked Ollama
  server runs in-process on the test's runtime, and the blocking `std::net::TcpStream`
  reads these tests do would otherwise block the single-threaded runtime, so the mock
  could never answer and every test needing an LLM call timed out.

## LLM Integration

### Event-Based Processing

SMB operations trigger `SMB_OPERATION_EVENT`:

```json
{
  "operation": "read",
  "params": {
    "path": "/documents/readme.txt",
    "offset": 0,
    "length": 4096
  }
}
```

LLM receives:

- Operation name (session_setup, create, read, write, etc.)
- Structured parameters (paths, offsets, sizes)

### Actions the model can return

Eight sync actions, and **every one has an executor branch** in `mod.rs`. That was not
true before: `smb_write_file`, `smb_create_file`, `smb_delete_file`,
`smb_create_directory` and `smb_delete_directory` were all declared in
`get_sync_actions()` with no arm that read them, so a model emitting any of the five got
silence. The two delete actions were removed (see below); the other three are routed.
`tests/server/smb/e2e_test.rs::smb_declared_actions_are_all_routed` fails the build if the
declared set and the event's action list drift apart again.

| `operation` | Expected action | Effect on the wire |
|---|---|---|
| `session_setup` | `smb_auth_success` / `smb_auth_deny` | STATUS_SUCCESS with a guest or null session, or STATUS_ACCESS_DENIED (`0xC0000022`) and the session forgotten |
| `create` | `smb_create_file` / `smb_create_directory` | FILE_ATTRIBUTE_NORMAL (0x80) or FILE_ATTRIBUTE_DIRECTORY (0x10) in the CREATE response, and the handle is recorded as one or the other; `smb_create_file`'s optional `size` becomes EndOfFile. **Neither action ⇒ STATUS_ACCESS_DENIED and no handle.** A directory where the client's CreateOptions demanded a non-directory (or the reverse) is STATUS_FILE_IS_A_DIRECTORY / STATUS_NOT_A_DIRECTORY. With `delete_on_close: true` in the event the open is a delete, and admitting it approves the delete |
| `read` | `smb_read_file` | the decoded `content` is the whole file; the READ response carries the requested range, and a read at or past its end is STATUS_END_OF_FILE; **absent action ⇒ STATUS_ACCESS_DENIED**. A READ on a directory handle is STATUS_INVALID_DEVICE_REQUEST without asking |
| `write` | `smb_write_file` | STATUS_SUCCESS with `bytes_written`; **absent action ⇒ STATUS_ACCESS_DENIED** |
| `query_info` | `smb_get_file_info` | `size` (and `modified_time`) in the file information classes that need a size (Standard, All, NetworkOpen, Stream), asked once per handle; **absent action ⇒ STATUS_ACCESS_DENIED**. Asked only for a file handle whose size is not yet known — a directory, a `size` from CREATE, or a class that needs no size is answered from the handle |
| `query_directory` | `smb_list_directory` | the `files` array (plus `.` and `..`), filtered by the client's search pattern, becomes the listing, handed out across as many calls as the client's buffer needs and then STATUS_NO_MORE_FILES; **absent action ⇒ STATUS_ACCESS_DENIED** |

The `create` event also carries `disposition` (`open`, `create`, `open_if`, …) and
`directory_requested`, and `delete_on_close: true` when the client opened in order to delete;
`query_directory` carries `pattern`; `write` carries `offset`, `data` and `encoding`. Paths are `/`-rooted at the share
and `/`-separated whatever the client sent; an empty CREATE name is the share root, `/`.

**Give `smb_create_file` a `size` whenever the file has content.** smbprotocol reads exactly
the CREATE response's EndOfFile and never asks again, so without it a file reads as empty.
smbclient asks through QUERY_INFO, so it works either way.

**`smb_delete_file` / `smb_delete_directory` do not exist, and deletes still reach the
model.** SMB2 has no DELETE command. A client deletes in one of two ways: it opens with
`FILE_DELETE_ON_CLOSE` and closes (smbclient's `rm` and `rmdir`), or it issues SET_INFO with
FileDispositionInformation (MS-SMB2 2.2.39; smbprotocol's `remove()`). The first is a CREATE,
and the event carries `delete_on_close: true` so the model's `smb_create_file` /
`smb_create_directory` is the approval — answering neither refuses it. The second is refused
STATUS_NOT_SUPPORTED, because SET_INFO is not implemented.

The option used to be ignored: the model was asked an ordinary "create", said yes, and
smbclient was told the file was gone while the model still believed it existed.
`real_client_test.rs` runs smbclient's `rm` and asserts the delete reached the model.

**Every operation is fail-closed.** An operation whose LLM response contains no
corresponding action is refused with STATUS_ACCESS_DENIED. Silence from the model, an LLM
outage and an explicit denial must not be indistinguishable from approval (see the fail-open
note in the root `CLAUDE.md`).

Write was fail-closed first; `create` and `read` were not, and both were the fail-open shape:

- **CREATE** read only `smb_create_directory` from the answer and treated its absence as
  "regular file", so an answer with *no* create action at all — a model that refused, a
  static handler with an empty list, a reply that deserialised but said nothing — returned
  STATUS_SUCCESS **and a live file handle**. Opening a handle is an access decision, so it
  now requires an affirmative `smb_create_file` or `smb_create_directory`.
- **READ** with no `smb_read_file` returned STATUS_SUCCESS whose body was the literal bytes
  `File not found or empty` — a *successful* read of fabricated content, indistinguishable
  from a file that genuinely holds that text.
- **QUERY_INFO** with no `smb_get_file_info` fell through to a hardcoded 4096, so silence
  produced STATUS_SUCCESS and a fabricated stat.
- **QUERY_DIRECTORY** with no `smb_list_directory` fell through to `unwrap_or_default()`, so
  silence produced STATUS_SUCCESS and an empty listing — which reads to a client as the
  positive assertion "this directory is empty". A genuinely empty directory is still
  expressible: `smb_list_directory` with an empty `files` array is a decision, not a silence.

Two more admission holes closed in the same pass, both of which made the model's answer
decorative rather than binding:

- **The denial did not deny.** `build_auth_denied_response` sent `0xC0000016` under a comment
  calling it `STATUS_ACCESS_DENIED`. `0xC0000016` is `STATUS_MORE_PROCESSING_REQUIRED` — the
  status a server sends *mid*-SPNEGO to mean "keep going". A real client reads it as an
  intermediate success and sends another SESSION_SETUP, so both `smb_auth_deny` and the
  fail-closed LLM-error branch reduced to "continue negotiating". `STATUS_ACCESS_DENIED` is
  `0xC0000022`, which the same file already defined and which the CREATE/READ/WRITE refusals
  already used. Two tests pinned the wrong constant while their assertion messages named
  ACCESS_DENIED, and a third accepted `0xC0000016` as a *successful* auth — so the same value
  stood for approval and refusal and nothing could tell them apart.
- **Nothing consulted the session.** `SmbConnectionState.sessions` was written by the
  successful auth path and read by nothing but a log line, so the decision governed exactly
  one response. A peer could open a socket and send CREATE or READ as its first bytes — no
  NEGOTIATE, no SESSION_SETUP, no admission event — and be served; a peer whose login had
  just been denied could send CREATE on the same connection and be served identically.
  Everything but NEGOTIATE, SESSION_SETUP, ECHO and CANCEL answers
  `STATUS_USER_SESSION_DELETED` (`0xC0000203`) unless the header's SessionId names a session
  the model admitted, logged `decision=fail_closed_no_session`; a command addressed to a
  share also needs a TreeId that session connected (STATUS_NETWORK_NAME_DELETED otherwise).
  `test_smb_file_operation_without_a_session_is_refused` pins it, and pins that the model is
  not consulted about the refused operation either.

**`allow_auth` is gone.** The auth check accepted `smb_auth_success` *or* `allow_auth` — a
name no `ActionDefinition` produces, absent from `get_sync_actions()` and from the event's
action list, and explicitly excluded by `smb_declared_actions_are_all_routed`. It was a
second, undocumented way to authenticate.

No wire response can carry the difference between "the model refused" and "the model said
nothing", so the log does: `decision=model_reject` when the answer had actions but none of
the expected type, `decision=fail_closed_no_action` when it had none at all,
`decision=fail_closed_llm_error` when the backend itself failed, and
`decision=fail_closed_no_session` for an operation that arrived before any authentication.
Every one of the six decided operations logs the first three with the path or user it
refused; `tests/server/smb/failure_modes_test.rs` drives all eighteen combinations and asserts
the status on the wire and the line in the log. Writing it found WRITE's refusal logging no
token at all.

### Payload encoding (read before writing prompts)

SMB carries file contents, which are routinely not text, so **both directions carry an
explicit `encoding` field** beside the payload string. There is no sniffing: `"SGVsbG8="`
is simultaneously valid text and valid base64, and only the sender knows which it means.

| Direction | Field | `encoding` values |
|---|---|---|
| Outbound (`smb_read_file`) | `content` | omitted / `"utf8"` (characters as-is, the default), `"base64"`, `"hex"` |
| Inbound (`smb_operation` for `write`) | `data` | `"utf8"` when every byte is printable ASCII, otherwise `"base64"` |

The pair is a bijection (`decode_smb_payload` / `encode_smb_payload` in `actions.rs`,
pinned by `smb_payload_encoding_round_trips`): pass a write event's `data` and `encoding`
straight into `smb_read_file` and the same bytes come back.

**The defect this replaced** is the reference case in the root `CLAUDE.md`.
`smb_read_file.content` was documented as "base64 encoded for binary" in two places while
the executor did `.as_str()…as_bytes()`, so a model that followed the documentation
delivered literal base64 ASCII as the file's contents. The inbound half was worse than
asymmetric: it used `String::from_utf8_lossy`, replacing every non-UTF-8 byte with U+FFFD,
so a written binary payload could not be echoed back even in principle. Undecodable
`content` now fails the READ with STATUS_DATA_ERROR rather than putting the raw string on
the wire.

**Example** — a binary file:

```json
{"type": "smb_read_file", "path": "/documents/icon.png",
 "content": "iVBORw0KGgo=", "encoding": "base64"}
```

Same string without `"encoding"` delivers the twelve characters `iVBORw0KGgo=`.

### Wire-format bugs fixed alongside the encoding work

All four were found by writing the first test that asserts response *bytes* rather than
"the server answered":

- **`blocking_lock()` in an async task.** `build_session_setup_response_with_user` and
  `build_tree_connect_response` called `tokio::sync::Mutex::blocking_lock()`, which panics
  when called from a runtime thread. Every SESSION_SETUP therefore killed its connection
  task the moment the LLM approved the login; `tokio::spawn` swallowed the panic, so the
  server stayed `Running`, the log showed the auth succeeding, and the client hung until
  its own timeout. Both are now `async fn` using `.lock().await`.
- **READ response `DataOffset` did not point at the data.** The body wrote four extra
  Reserved bytes after `DataOffset`, so the payload started at 84 while the response
  advertised 80. A client reading at the offset the server declared got four zero bytes
  and a truncated file.
- **WRITE `Length` read from the wrong offset.** MS-SMB2 2.2.21 puts it at body offset 4;
  the code read offset 0, which is `StructureSize`+`DataOffset` (0x00700031 for a
  well-formed request) — so the first WRITE blocked in `read_exact` waiting for 7 MB that
  never arrived. The data is now located through `DataOffset` inside the frame, and the
  length is capped at `MAX_WRITE_SIZE` (see "Inbound size bounds").
- **CREATE file name located by a hardcoded offset.** `parse_smb2_path` indexed the
  body-relative slice at 120, which is the *absolute* offset of the name buffer, so for a
  well-formed request it read 64 bytes past the name and every CREATE resolved to
  `/unknown`. It now honours `NameOffset`/`NameLength`.

### Error Handling

There is no explicit error field: the model refuses by omitting the expected action, and the
server answers STATUS_ACCESS_DENIED. An unknown SMB2 command and a request body too short to
carry its own fields now answer `STATUS_NOT_SUPPORTED` and `STATUS_INVALID_PARAMETER`; all
three used to return no reply at all, leaving the peer to wait out its own timeout on a
connection the unread body had already desynced.

### When the LLM call itself fails

All six `consult_llm` call sites answer in SMB2 rather than dropping the connection. Five of
them used to propagate the error with `?`, which broke out of the connection loop and closed
the socket; a client cannot tell that from a hung server and simply waits out its own
timeout.

| Site | Response on LLM failure |
|---|---|
| `session_setup` | STATUS_ACCESS_DENIED (auth is denied, never granted) |
| `create` / `read` / `write` / `query_info` / `query_directory` | SMB2 ERROR response (MS-SMB2 2.2.2) for that command |

The NTSTATUS is `STATUS_INSUFFICIENT_RESOURCES` (0xC000009A) when
`crate::llm::is_overload_error` identifies capacity exhaustion - the closest NTSTATUS to
"retryable" - and `STATUS_INTERNAL_ERROR` (0xC00000E5) otherwise
(`status_for_llm_failure`, a pure function tested for every `RateLimitError`, bare and under
the context layer `call_llm` adds). Both stay distinguishable
from the model's own refusal (STATUS_ACCESS_DENIED on a write) and from an undecodable
payload (STATUS_DATA_ERROR on a read), which is the point: an outage must never look like a
decision, and a `query_directory` failure must not be answered with an empty listing that
reads as "the directory is empty".

Every reply echoes the request's **MessageId, TreeId and SessionId**, because every header is
built by `ResponseHeader::for_request` + `encode`. A client correlates replies to outstanding
requests by MessageId, so a reply carrying the wrong one is discarded and the client is back
to waiting out its timeout — which is exactly what eight builders did while each laid its own
header out and put the MessageId at offset 20. `header_layout_test.rs` calls every builder and
checks every field's offset; moving the MessageId back to 20 fails it, fails the pcap oracle
("Malformed Packet" on every reply after NEGOTIATE), and fails smbclient at session setup.

The connection is *not* torn down: the error ends the operation, not the session, and a
following CLOSE is still answered. `tests/server/smb/llm_failure_test.rs` asserts this on the
response bytes for CREATE and READ; `failure_modes_test.rs` covers the other four.

## Example Prompts and Responses

### Example 1: Basic File Server

**Prompt:**

```
Start an SMB file server on port 445. Accept all guest connections.
Provide /documents directory with readme.txt (content: "Welcome to NetGet SMB").
```

**LLM Response (session_setup):**

```json
{
  "actions": [
    {
      "type": "smb_auth_success"
    }
  ]
}
```

**LLM Response (read):**

```json
{
  "actions": [
    {
      "type": "smb_read_file",
      "path": "/documents/readme.txt",
      "content": "Welcome to NetGet SMB",
      "encoding": "utf8"
    }
  ]
}
```

### Example 2: Authentication Control

**Prompt:**

```
Start an SMB file server on port 445. Only allow user "alice" to authenticate.
Deny all other users.
```

> The user name is the one the client's NTLMSSP AUTHENTICATE carries (`smbclient -U alice`),
> so a per-user policy works — as a policy about names. **No password is verified**
> (`password_verified: false` in the event), so it admits anyone who types `alice`; the
> session is a guest session either way. A one-step SESSION_SETUP with an empty security
> buffer always reports `"guest"`.

**LLM Response (alice):**

```json
{
  "actions": [
    {
      "type": "show_message",
      "message": "Allowing alice to connect"
    },
    {
      "type": "smb_auth_success"
    }
  ]
}
```

**LLM Response (bob):**

```json
{
  "actions": [
    {
      "type": "show_message",
      "message": "Denying bob - not authorized"
    },
    {
      "type": "smb_auth_deny"
    }
  ]
}
```

### Example 3: Directory Listings

**Prompt:**

```
Start an SMB file server on port 445. /documents contains: report.pdf (1024 bytes),
presentation.pptx (4096 bytes), archive folder.
```

**LLM Response (query_directory):**

```json
{
  "actions": [
    {
      "type": "smb_list_directory",
      "files": [
        {"name": "report.pdf", "size": 1024, "is_directory": false},
        {"name": "presentation.pptx", "size": 4096, "is_directory": false},
        {"name": "archive", "size": 0, "is_directory": true}
      ]
    }
  ]
}
```

### Example 4: Write Operations

**Prompt:**

```
Start an SMB file server on port 445. Accept file writes, log the content.
```

**LLM Response (write):** the write is refused unless `smb_write_file` is returned.

```json
{
  "actions": [
    {
      "type": "show_message",
      "message": "Client wrote 256 bytes to /documents/newfile.txt"
    },
    {
      "type": "smb_write_file",
      "path": "/documents/newfile.txt"
    }
  ]
}
```

## References

- [MS-SMB2: Server Message Block (SMB) Protocol Versions 2 and 3](https://docs.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2)
- [SMB2 Wikipedia](https://en.wikipedia.org/wiki/Server_Message_Block#SMB_2.0)
- [Samba SMB Implementation](https://www.samba.org/)
- [SMB Packet Structure](https://wiki.wireshark.org/SMB2)

## Logging

Dual logging (tracing macros and `status_tx`) through `crate::logging::emit::Log`. What is
actually written:

- **INFO** - connection accepted, opened and closed; `SMB2 SESSION_SETUP for user: …`;
  authentication granted; TREE_CONNECT, CREATE, READ, WRITE, QUERY_INFO, QUERY_DIRECTORY and
  CLOSE with their path; each message the model's answer produced.
- **WARN** - every refusal, tagged with its `decision=` token (see above and "Connection
  bounds"); a first frame that is not SMB2 (including SMB1); a transport byte that is not a
  Direct TCP session message; an unsupported command.
- **DEBUG** - the command code of every request, the dialect chosen, NTLMSSP legs, allocated
  handles, END_OF_FILE reads, the number of actions the model returned.
- **TRACE** - the accept loop and the size of each response written. No packet is hex-dumped.
- **ERROR** - read and write failures on the socket, and a handler that returned `Err`.

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever, and a
hundred of them was a free denial of service on a server that would happily accept a hundred
more. It now declares both halves; the constants and the reasoning live beside them in
`src/server/smb/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_MESSAGE_READ_TIMEOUT` (`first_byte_timeout_secs`) | 30s | SMB2 is client-speaks-first: NEGOTIATE is the first message, and `smbclient`, the Windows redirector and `mount -t cifs` all send it inside the connect path. It governs until a session is **admitted**, not until the first byte. |
| `IDLE_BETWEEN_MESSAGES_TIMEOUT` (`idle_timeout_secs`) | 900s | Not a number invented here: it is Windows' `autodisconnect` default — the interval after which a server disconnects an idle SMB session. A mounted share with no I/O is genuinely idle for long stretches and must not be torn down for it. |
| `BODY_READ_TIMEOUT` (`body_timeout_secs`) | 30s | A different claim, and much shorter: the peer's transport header has said "this many bytes are coming" and the server has already allocated for them. Every read of an announced frame goes through `read_body_exact`. |
| `MAX_CONNECTIONS` | 256 | Refusal: **a plain close**, as a real SMB server does. Every SMB2 response echoes the request's MessageId, TreeId and SessionId, and a refused peer has sent no request to echo. Samba past `max smbd processes` and Windows past its connection limit both close without a message. The permit is held by the connection's task, which owns the read loop, so a connection the *server* closes returns its slot even while the peer keeps its end open. |
| `MAX_SESSIONS_PER_CONNECTION` | 16 | The first NTLMSSP leg opens a session before anyone is asked, so without it a peer grows the table a leg at a time. Counts mid-exchange and admitted sessions together. `STATUS_INSUFFICIENT_RESOURCES`, `decision=fail_closed_session_cap`, before the model (a one-step guest login at the cap costs no call). |
| `MAX_TREES_PER_CONNECTION` | 64 | TREE_CONNECT asks no one. `STATUS_INSUFFICIENT_RESOURCES`, `decision=fail_closed_tree_cap`. |
| `MAX_OPEN_FILES_PER_CONNECTION` | 1024 | A static handler answering every CREATE stands in front of nothing else. `STATUS_TOO_MANY_OPENED_FILES` (Samba's answer past `max open files`), `decision=fail_closed_open_file_cap`, before the model. |
| `MAX_COMPOUND_REQUESTS` | 32 | A 1 MiB frame links ~17 000 headers, each a possible model call. Requests past 32 are answered `STATUS_INSUFFICIENT_RESOURCES` to their own MessageId without being acted on, `decision=fail_closed_compound_cap`. Real clients compound fewer than ten. |
| `wire::MAX_CREDIT_GRANT` | 64 | **Outbound.** A response grants the request's `CreditRequest`, at least 1 and at most 64, so a pipelining client stays supplied and no single request inflates its window without limit. |
| RFC 1002 session request | 256 bytes of names | The one frame read before SMB2 on a connection that dialled 139. Longer is not a session request this server reads, and closes the connection. |
| `MAX_RESPONSE_FRAME_BYTES` | 2^24 - 1 less 2 MiB | **Outbound.** A Direct TCP header cannot announce more than 2^24 - 1 bytes and 17 READs of `MaxReadSize` ask for 17 MiB; before this bound the reply hit `wire::frame`'s assertion and the connection task panicked. A response that would take the compound reply past it is `STATUS_INSUFFICIENT_RESOURCES`, `decision=fail_closed_response_too_large`; the 2 MiB is what the refusals themselves can cost. |

**NetGet's own SMB client is *lazy*, so it is never the silent peer this bound closes:**
`src/client/smb/mod.rs` only initialises a libsmbclient context, and NEGOTIATE goes out inside
the library on the first `opendir`/`open` an action runs — `PROTOCOL_QUALITY.md`'s three-state
test.

**The deadline covers the read and nothing else.** The deadline wraps the `read()` call in this protocol's own loop, and everything that can legitimately take minutes happens after it returns. The LLM round-trip, and a `manual`
rule parking an event for a human (`src/state/intercepts.rs`, 300s by default), are outside
every deadline here, so an answer that takes minutes can never close the connection it is an
answer for. That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse:
TFTP evicted live transfers because "idle" was measured wrongly.

`tests/server/smb/bounds_test.rs` drives every bound in this table from the wire — each
deadline at its configured value (and the first-message one at its 30s default), the
connection cap with a slot returned by a server-side close, each per-connection table at the
cap and one past it with the slot coming back, and both compound bounds — and each was verified
by removal. `tests/tcp_server_bounds_ratchet_test.rs` additionally fails the build if the
deadline or the cap disappears from the source.

## Maturity: Beta, and what it rests on

**Beta since September 2026**, on the bar in the root `CLAUDE.md`: two independent
third-party clients complete a real session, in tests that fail rather than skip when the
client is absent (`tests/server/smb/real_client_test.rs`).

| Client | What it drives | What it checks that nothing else does |
|---|---|---|
| Samba `smbclient` 4.24 (C) | anonymous SPNEGO/NTLMSSP login, TREE_CONNECT, `ls` (QUERY_DIRECTORY to NO_MORE_FILES, FileFsFullSizeInformation), `get` of a 70 000-byte binary file (CREATE, QUERY_INFO FileAllInformation, READ, CLOSE), TREE_DISCONNECT, LOGOFF | MessageId correlation on every reply, credits, the NTLMSSP CHALLENGE's shape, directory-entry chaining, the byte-exact file |
| `smbprotocol` 1.17 (Python) | named-guest bare-NTLMSSP login, listdir, open+read of the same file | that a client which trusts CREATE's EndOfFile reads the whole file |

The smbclient session is recorded through a relay and read clean by Wireshark's `nbss`/`smb2`
dissectors (the pcap oracle); so is a raw session exercising every implemented command
(`header_layout_test.rs`). Both inbound bounds have a test that was verified by removal.

**Not Stable, and what is missing for it:** no fuzz target exists for the request parsers or
the NTLMSSP parser (condition 3); `FIRST_MESSAGE_READ_TIMEOUT`, `IDLE_BETWEEN_MESSAGES_TIMEOUT`,
`BODY_READ_TIMEOUT` and `MAX_CONNECTIONS` are held by the shared ratchets but have no SMB test
of their own (condition 4). **What the rating covers is the surface the server implements** —
SMB 2.0.2/2.1, guest and null sessions, the commands above — which is a small subset of
MS-SMB2. Neither client exercises WRITE; the raw-packet suites do. Windows, macOS and Linux
kernel clients have not been run against it.
