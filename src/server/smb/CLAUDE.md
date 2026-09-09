# SMB Protocol Implementation

## Overview

SMB2 (Server Message Block version 2) file server implementing a subset of MS-SMB2 protocol. Provides Windows-compatible
file sharing where the LLM controls the virtual filesystem, authentication, and file operations.

**Protocol**: SMB 2.1 (dialect 0x0210)
**Transport**: raw SMB2 over TCP, with **no NetBIOS session-service framing**. This is not
"direct TCP (445) or NetBIOS over TCP (139)" as this line used to claim: every real client on
either port prefixes each message with the 4-byte NBSS header (`00` + a 24-bit length), and
this server reads 64 bytes and requires `\xFESMB` at offset 0. A real `smbclient` therefore
hands it `00 00 00 xx`, the signature check fails, and the connection closes with no reply.
The tests speak the same unframed dialect the server does, which is why nothing caught it.
**Port**: 445 (standard), configurable
**Status**: Experimental
**Startup parameters**: none declared, none read.

## Library Choices

- **Manual SMB2 implementation** - No library used
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

Implements minimal SMB 2.1 subset:

- **Negotiate Protocol** - Offer SMB 2.1 dialect (0x0210)
- **Session Setup** - Guest authentication only
- **Tree Connect** - accepted unconditionally, with **no LLM call and no share check**, and
  the arm does not consume the request body, so the next header read finds the leftover
  bytes and drops the connection. No test sends TREE_CONNECT. Treat it as unimplemented.
- **Create** - Open/create files and directories
- **Read/Write** - File content operations
- **Close** - Close file handles
- **Query Info** - File attributes
- **Query Directory** - Directory listings

**Not implemented**:

- SMB 3.x features (encryption, multichannel, etc.)
- NTLM authentication (only guest)
- Opportunistic locks (oplocks)
- Durable handles
- Compound requests

### LLM-Controlled Filesystem

Similar to NFS, LLM controls entire filesystem:

- **Authentication** - LLM decides who can connect
- **File operations** - LLM provides file content, attributes
- **Directory structure** - LLM defines folders and files

### Guest-Only Authentication

Current implementation uses guest authentication:

- No password verification
- LLM can accept or deny based on username
- Session IDs allocated per connection

### File Handle Management

Server maintains file handle state:

- 16-byte GUID per file handle (generated with timestamp)
- HashMap of handles → file paths
- Handles tracked per connection

### Binary Protocol Handling

Manual SMB2 packet parsing:

- 64-byte SMB2 header parsing
- Command extraction (offset 12-13, little-endian u16)
- Response builders for each command type

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
- Protocol-specific info: **none**. This line used to name a
  `ProtocolConnectionInfo::Smb { authenticated, username, session_id, open_files }` variant;
  no such variant exists (`ProtocolConnectionInfo` is a generic JSON wrapper), the connection
  is registered with `ProtocolConnectionInfo::empty()`, and the update site is a `TODO`.
- Stats: bytes_sent, bytes_received, packets_sent, packets_received
- Status updated on connection close

### Per-Connection State

`SmbConnectionState` maintains:

- **Sessions**: HashMap<session_id, SmbSession>
- **Trees**: HashMap<tree_id, SmbTreeConnect>
- **Files**: HashMap<file_handle, SmbFileHandle>
- Next session ID, next tree ID generators

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

Per-connection state in `Arc<Mutex<SmbConnectionState>>`:

- Sessions: Maps session_id → username, authenticated flag
- Trees: Maps tree_id → share name
- Files: Maps file_id (GUID) → path, is_directory

### Filesystem State

LLM maintains filesystem via instructions:

- File paths stored in file handles
- LLM consulted for file content on demand
- No persistent storage

## Limitations

### Simplified SMB2 Implementation

- **SMB 2.1 only** - No SMB 3.x features
- **Guest auth only** - No NTLM, Kerberos, or secure authentication
- **No encryption** - Plain text protocol (no SMB3 encryption)
- **No signing** - Packets not cryptographically signed
- **No oplocks** - No opportunistic locking for performance

### Protocol Simplifications

- Fixed tree IDs and session IDs (not from request)
- Minimal header fields populated
- Timestamps often zero
- File attributes simplified

### LLM Performance

- **CRITICAL**: Every file operation calls LLM (slow)
- High latency (seconds per operation)
- Not suitable for real file sharing workloads
- Script and static handlers do work: `call_llm` dispatches them, so an
  `event_handlers` entry on `smb_operation` answers without a model round-trip

### Testing Limitations

- Real SMB clients (Windows, smbclient) have strict requirements
- Clients expect full SMB2 compliance
- Some clients probe for SMB1 (not supported)
- **Testing uses raw TCP sockets, not real SMB clients.** `metadata().e2e_testing` used to
  claim "smbclient / Windows Explorer"; neither has ever been run against this server, and
  the claim now says so.
- The tests use `#[tokio::test(flavor = "multi_thread")]`. They must: the mocked Ollama
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
| `session_setup` | `smb_auth_success` / `smb_auth_deny` | STATUS_SUCCESS with a session id, or STATUS_ACCESS_DENIED (`0xC0000022`) |
| `create` | `smb_create_file` / `smb_create_directory` | FILE_ATTRIBUTE_NORMAL (0x80) or FILE_ATTRIBUTE_DIRECTORY (0x10) in the CREATE response, and the handle is recorded as one or the other; **neither action ⇒ STATUS_ACCESS_DENIED and no handle** |
| `read` | `smb_read_file` | the decoded `content` becomes the READ response body; **absent action ⇒ STATUS_ACCESS_DENIED** |
| `write` | `smb_write_file` | STATUS_SUCCESS with `bytes_written`; **absent action ⇒ STATUS_ACCESS_DENIED** |
| `query_info` | `smb_get_file_info` | `size` in the QUERY_INFO response; **absent action ⇒ STATUS_ACCESS_DENIED** |
| `query_directory` | `smb_list_directory` | the `files` array becomes the directory listing; **absent action ⇒ STATUS_ACCESS_DENIED** |

**`smb_delete_file` / `smb_delete_directory` do not exist.** SMB2 has no DELETE command:
a client deletes by opening the file and issuing SET_INFO with
FileDispositionInformation (MS-SMB2 2.2.39). This server does not implement SET_INFO at
all, so neither action could ever have been requested. Implementing delete means
implementing SET_INFO first.

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
  Everything but NEGOTIATE and SESSION_SETUP now answers `STATUS_USER_SESSION_DELETED`
  (`0xC0000203`) until a session exists, logged `decision=fail_closed_no_session`.
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
SESSION_SETUP had none of these until this pass — the one place the distinction matters
most.

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
  never arrived. The length is now also capped at 8 MiB before allocating.
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
"retryable" - and `STATUS_INTERNAL_ERROR` (0xC00000E5) otherwise. Both stay distinguishable
from the model's own refusal (STATUS_ACCESS_DENIED on a write) and from an undecodable
payload (STATUS_DATA_ERROR on a read), which is the point: an outage must never look like a
decision, and a `query_directory` failure must not be answered with an empty listing that
reads as "the directory is empty".

`build_error_response` echoes the request's **MessageId, TreeId and SessionId**. A client
correlates replies to outstanding requests by MessageId, so an error carrying the wrong one
is discarded and the client is back to waiting out its timeout. It also lays the 64-byte
header out per MS-SMB2 2.2.1.2; it previously wrote MessageId at offset 20 (omitting
NextCommand) and hardcoded TreeId/SessionId to 1.

The connection is *not* torn down: the error ends the operation, not the session, and a
following CLOSE is still answered. `tests/server/smb/llm_failure_test.rs` asserts all of
this on the response bytes.

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

> **This example does not work as written.** `parse_smb2_username` reads the SESSION_SETUP
> body from `body.iter().take(200).skip(24)` while the caller reads exactly 24 bytes into a
> fixed `[u8; 24]`, so the iterator is always empty and the username is always `"guest"`.
> Per-user policy is therefore unimplementable today, and `smb_auth_success.username` is
> declared `required: true` and then ignored by the executor. Fixing it means parsing the
> real NTLMSSP security buffer.

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

### Structured Logging Levels

**TRACE** - Full SMB2 packet details:

- Hex dump of request/response packets
- Detailed header parsing
- File handle mappings

**DEBUG** - SMB2 command summaries:

- Command type and parameters
- "SMB2 CREATE /documents/readme.txt"
- "SMB2 READ fileid=0x123... offset=0 len=4096"

**INFO** - High-level events:

- Connection open/close
- Authentication attempts
- "SMB connection from 192.168.1.100"
- "SMB auth attempt: guest"
- "SMB connection closed"

**WARN** - Non-fatal issues:

- Invalid SMB2 signature
- Unknown command codes
- Malformed requests

**ERROR** - Critical failures:

- LLM communication errors
- Connection read/write failures
- Invalid packet structure

All logs use dual logging pattern (tracing macros + status_tx).
