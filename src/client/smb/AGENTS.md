# SMB Client Implementation

## Overview

SMB/CIFS client for accessing Windows file shares and Samba servers. Supports file operations (read, write, delete),
directory operations (list, create, delete), and authentication.

## Library Choice

**Primary Library:** `pavao` v0.2.13 (libsmbclient wrapper)

**Rationale:**

- Wraps the mature libsmbclient library (from Samba project)
- Supports SMB 1/2/3 protocol versions with automatic negotiation
- Simple API for file operations (list_dir, read, write, mkdir, rmdir, unlink)
- Handles authentication (username/password, domain/workgroup)
- Well-tested in production Samba deployments

**Dependencies:**

- System library: `libsmbclient` (from samba-common package)
- Rust crate: `pavao` (safe Rust bindings)

**Alternative Considered:**

- `smbc` crate: Similar libsmbclient wrapper with std::fs-like interface
- Chose `pavao` for SMB 2/3 support and simpler API

## Architecture

### Connection Model

Unlike TCP-based clients, SMB client doesn't maintain a persistent connection. Instead:

1. **Client Context**: Created once with credentials (`SmbClient::new()`)
2. **Operations**: Each operation (list, read, write) establishes connection as needed
3. **Automatic Reconnection**: Library handles connection management internally

### LLM Integration

**Event-Driven Model:**

```
1. Client initialization -> SMB_CLIENT_CONNECTED_EVENT
2. LLM responds with action (e.g., list_directory)
3. Execute action -> SMB_CLIENT_DIR_LISTED_EVENT
4. LLM analyzes results and responds with next action
5. Repeat until disconnect
```

**Key Differences from Stream-Based Clients:**

- No read loop (operations are synchronous)
- Each operation triggers LLM call with result event
- Actions executed sequentially (not pipelined)

### State Management

**Client State:** Tracked in AppState as `ClientStatus`:

- `Connecting`: Initial state
- `Connected`: Client context created, ready for operations
- `Disconnected`: Client closed
- `Error`: Operation failed

**Memory:** Used to track:

- Current working directory
- Recently accessed files
- Operation history

## LLM Control Points

### Async Actions (User-Triggered)

1. **list_directory**: List contents of SMB directory
    - Parameters: `path` (smb://server/share/dir)
    - Returns: Array of entries with name, type, comment

2. **read_file**: Read file from SMB share
    - Parameters: `path` (smb://server/share/file.txt)
    - Returns: File content (text or base64 for binary)

3. **write_file**: Write file to SMB share
    - Parameters: `path`, `content`
    - Returns: Bytes written

4. **create_directory**: Create directory on SMB share
    - Parameters: `path`

5. **delete_file**: Delete file from SMB share
    - Parameters: `path`

6. **delete_directory**: Delete directory from SMB share
    - Parameters: `path`

7. **disconnect**: Close SMB client

### Sync Actions (Response Actions)

1. **list_directory**: List directory after previous operation
2. **read_file**: Read file after previous operation
3. **wait_for_more**: No-op (wait for user input)

## Events

### SMB_CLIENT_CONNECTED_EVENT

Triggered when SMB client initializes with credentials.

**Parameters:**

- `share_url`: Base SMB URL (smb://server/share)

**LLM Decisions:**

- List root directory
- Read specific file
- Navigate to subdirectory

### SMB_CLIENT_DIR_LISTED_EVENT

Triggered when directory listing completes.

**Parameters:**

- `path`: Directory path
- `entries`: Array of directory entries

**Entry Format:**

```json
{
  "name": "filename.txt",
  "type": "file|dir|link|file_share|printer_share|...",
  "comment": "SMB comment"
}
```

**LLM Decisions:**

- Read a file from listing
- Navigate to subdirectory
- Create/delete files

### SMB_CLIENT_FILE_READ_EVENT

Triggered when file read completes.

**Parameters:**

- `path`: File path
- `content`: File content (text or base64:...)
- `size`: File size in bytes

**Content Encoding:**

- UTF-8 text: Plain string
- Binary data: `base64:<encoded>`

**LLM Decisions:**

- Process file content
- Write modified version
- Read another file

### SMB_CLIENT_FILE_WRITTEN_EVENT

Triggered when file write completes.

**Parameters:**

- `path`: File path
- `bytes_written`: Number of bytes written

**LLM Decisions:**

- Read back written file
- Write another file
- List directory to confirm

### SMB_CLIENT_ERROR_EVENT

Triggered when SMB operation fails.

**Parameters:**

- `error`: Error message
- `operation`: Operation that failed

**Common Errors:**

- Authentication failure (invalid credentials)
- Permission denied (no write access)
- File not found
- Network error

**LLM Decisions:**

- Retry operation
- Try alternative path
- Report error to user

## Authentication

**Startup Parameters:**

```json
{
  "username": "guest",
  "password": "",
  "domain": "WORKGROUP",
  "workgroup": "WORKGROUP"
}
```

**Default:** Guest access (username=guest, password=empty)

**Credentials Handling:**

- Passed to `SmbCredentials::new()`
- Stored in SMB client context
- Used for all operations

**Domain vs Workgroup:**

- Domain: Windows domain (e.g., CORP)
- Workgroup: Workgroup name (e.g., WORKGROUP)
- Typically one or the other is specified

## URL Format

**SMB URLs:** `smb://server/share/path/to/file`

**Examples:**

- List share: `smb://192.168.1.100/public/`
- Read file: `smb://fileserver/documents/readme.txt`
- Nested path: `smb://server/data/2024/01/report.pdf`

**Important:**

- Must use `smb://` prefix
- Server can be hostname or IP
- Share name is required
- Path separators: `/` (Unix-style)

## Limitations

1. **System Dependency**: Requires libsmbclient system library
    - Linux: `apt install libsmbclient-dev` or `yum install samba-client`
    - macOS: `brew install samba`
    - Not cross-platform (needs Samba libraries)

2. **Synchronous Operations**: Each operation blocks until complete
    - No concurrent operations on same client
    - Large file transfers block LLM interaction

3. **No Streaming**: Files read/written entirely into memory
    - Large files (>100MB) may cause memory pressure
    - No progress updates during transfer

4. **Limited Metadata**: `list_dir` returns basic info
    - File size not included in directory listing
    - Use `stat()` for detailed attributes (not exposed yet)

5. **Error Granularity**: Errors from libsmbclient can be cryptic
    - Error messages may not be actionable
    - LLM may struggle to interpret low-level errors

6. **SMB Version**: Automatic negotiation (SMB 1/2/3)
    - Modern servers prefer SMB 2/3
    - SMB 1 deprecated (security concerns)
    - No manual version selection exposed

## Security Considerations

1. **Credentials in Memory**: Username/password stored in SmbCredentials
    - Not encrypted in memory
    - Visible in process memory
    - Consider using Kerberos (not implemented yet)

2. **Network Exposure**: SMB traffic on port 445
    - Use VPN or trusted network
    - Consider SMB over SSH tunnel

3. **Guest Access**: Default guest credentials
    - Read-only on most servers
    - May be disabled on secure servers

## Testing Strategy

See `tests/client/smb/CLAUDE.md` for E2E testing approach.

**Test Server:** Docker Samba container

- Easy setup with `dperson/samba` image
- Configurable shares and permissions
- Supports guest and authenticated access

## Future Enhancements

1. **Streaming API**: Implement chunked read/write for large files
2. **Metadata Operations**: Expose `stat()` for file attributes (size, mtime, permissions)
3. **Kerberos Auth**: Support for AD-integrated authentication
4. **SMB Version Control**: Allow manual SMB protocol version selection
5. **Recursive Operations**: Recursive directory listing and deletion
6. **Symlink Handling**: Follow or report symbolic links
7. **Progress Events**: Fire events during large file transfers

## Example Prompts

```
# List SMB share
Connect to SMB at //192.168.1.100/public (guest) and list the root directory

# Read file
Connect to SMB at //fileserver/documents (user: alice, pass: secret) and read readme.txt

# Write file
Connect to SMB at //server/upload and write "Hello from NetGet" to test.txt

# Recursive listing
Connect to SMB at //server/data and recursively list all subdirectories

# File search
Connect to SMB at //server/logs and find all .log files modified today
```

## Implementation Notes

**Dual Logging:** All operations use:

- `info!()`, `debug!()`, `error!()` -> `netget.log`
- `status_tx.send()` -> TUI display

**Error Handling:**

- Operations return `Result<T>`
- Errors trigger `SMB_CLIENT_ERROR_EVENT`
- LLM decides whether to retry or abort

**Memory Usage:**

- File content loaded entirely into memory
- Large files (>100MB) may cause issues
- Consider chunking for production use

**Concurrency:**

- Single SMB client context per client ID
- No concurrent operations (library not thread-safe)
- Operations executed sequentially

## References

- pavao crate: https://crates.io/crates/pavao
- libsmbclient: https://www.samba.org/samba/docs/current/man-html/libsmbclient.7.html
- SMB protocol: https://docs.microsoft.com/en-us/openspecs/windows_protocols/ms-smb/

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into the running client. `command_loop` in
`mod.rs` drains the bounded channel and runs each action through `apply_smb_result` — the
same function the `smb_client_connected` and follow-up event paths use.

The `pavao::SmbClient` handle lives behind an `Arc<Mutex<_>>` (`SharedSmb`) so both the LLM
task and the command loop can use it. libsmbclient is not thread-safe, so the mutex is the
serialisation the library requires anyway. **The guard is taken and released around each
synchronous pavao call and is never held across an `.await`** — in particular never across an
LLM round-trip, which a `*` -> manual rule can park for minutes.

The channel is registered **before** the connected-event LLM call.

Outcomes:

| action | `ClientSendOutcome` |
|---|---|
| `list_directory` | `Executed { detail: "list_directory \"…\": N entries" }` (or `… failed: <err>`) |
| `read_file` | `Executed { detail: "read_file \"…\": N bytes read" }` |
| `write_file` | `Executed { detail: "write_file \"…\": N bytes written" }` |
| `create_directory` / `delete_file` / `delete_directory` | `Executed { detail: "<verb> \"…\": created/deleted" }` |
| `disconnect` | `Disconnected` (handle dropped) |
| unknown verb | `Rejected { error }` |

**SMB never reports `Sent`.** libsmbclient owns the transport and may sign or encrypt it, so
NetGet never sees a wire byte count. A write reports the payload bytes it really put into the
file, inside `Executed`.

## Native ownership and bounded operations (October 2026 follow-up)

Initialization, all share operations, and context destruction run on blocking workers. The
async lock is acquired before spawning work; file handles are created and dropped entirely
inside the worker. Buffered file reads/writes are limited to 8 MiB; over-limit reads return an
error, never partial success. Native operation timeout is 30 seconds. Failed operations emit
their error event and return Err to injected callers rather than reporting Executed success.

The cached pavao 0.2.13 implementation has one process-global SMBCTX, and every SmbClient drop
frees it. NetGet therefore allows one active native SMB session per process and explicitly
refuses a second owner until cleanup finishes. This prevents one session invalidating another.
The installed native library is available to compile and run the pure bounded-reader regression;
a live authenticated share is still required to validate server-side behavior and cancellation of
an in-progress native operation cannot forcibly interrupt the C library.
