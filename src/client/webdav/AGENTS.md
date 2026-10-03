# WebDAV Client Implementation

## Overview

The WebDAV client extends HTTP to support Web-based Distributed Authoring and Versioning (WebDAV) operations. It
provides LLM-controlled access to remote file systems via WebDAV protocol.

## Library Choices

### Core Libraries

- **reqwest** (v0.12+) - HTTP client with custom method support
    - Supports custom HTTP methods via `Method::from_bytes()`
    - Built-in TLS support for HTTPS
    - Timeout configuration
    - No native WebDAV support, so we implement WebDAV on top

### Why reqwest?

- **Mature**: Battle-tested HTTP client library
- **Async**: Native tokio support
- **Custom Methods**: Supports WebDAV methods (PROPFIND, MKCOL, COPY, MOVE, etc.)
- **Simple**: Easy to construct requests with headers and bodies

## Architecture

### Connection Model

WebDAV is **connectionless** like HTTP - each operation is a separate HTTP request:

1. **Client Initialization**: resolves `default_headers` and `auth`, stores the base URL. It
   builds **no** HTTP client — see below.
2. **On-Demand Requests**: LLM triggers WebDAV methods via actions
3. **Response Processing**: LLM receives XML responses and decides next action

`perform_request` asks `http_client_for(base_url)`, which keeps one `reqwest::Client` per base
URL behind a `OnceLock` map and builds it on `spawn_blocking` the first time, through
`client_for_endpoint_with_timeout`. Building a reqwest client is a *blocking* operation (rustls
setup plus the platform root store, which on macOS is a synchronous keychain read), so the
earlier arrangement — one built per request on the runtime, plus a second built at connect and
dropped unused — parked a tokio worker on every call and skipped the literal-IP DNS bypass.

### State Management

State stored in `AppState::protocol_data`:

- `base_url`: Base URL for all WebDAV requests
- `http_client`: a literal `"initialized"` marker string. No client is stored here; the
  real ones live in the process-wide `HTTP_CLIENTS` map keyed by base URL.
- `startup_headers`: the `default_headers` startup parameter, plus the `authorization`
  header resolved from `auth`. Applied to every request **underneath** the per-request
  headers (Depth, Destination, Overwrite, Content-Type), merged on the lowercased name
  before anything is applied — `reqwest::RequestBuilder::header` appends, so applying both
  sets in turn would send a header twice.

### WebDAV Methods Supported

| Method        | Purpose                            | Body Required                |
|---------------|------------------------------------|------------------------------|
| **PROPFIND**  | List properties/directory contents | XML (properties to fetch)    |
| **MKCOL**     | Create collection (directory)      | No                           |
| **COPY**      | Copy resource                      | No (uses Destination header) |
| **MOVE**      | Move/rename resource               | No (uses Destination header) |
| **DELETE**    | Delete resource                    | No                           |
| **PUT**       | Upload file                        | Yes (file content)           |
| **GET**       | Download file                      | No                           |

`perform_request` can additionally *construct* PROPPATCH, LOCK and UNLOCK, but **no action
reaches them**: `execute_action` dispatches exactly `propfind`, `mkcol`, `copy`, `move`,
`delete`, `put`, `get`, `disconnect` and `wait_for_more`, and no `ActionDefinition` advertises
the other three. The model cannot ask for them and neither can an injected command, so those
three arms are dead code. They were listed here as supported methods.

## LLM Integration

### Action Flow

1. **User Request** → LLM instruction (e.g., "List files in /dav/documents/")
2. **LLM Decision** → Generates WebDAV action (e.g., PROPFIND with depth:1)
3. **Action Execution** → `execute_action()` returns `ClientActionResult::Custom`
4. **Request Processing** → `make_request()` constructs HTTP request with WebDAV method
5. **Response** → LLM receives XML response via `webdav_response_received` event
6. **Next Action** → LLM decides follow-up (e.g., download a file, create folder)

### Event Triggers

- **webdav_connected**: Fired when client initializes
- **webdav_response_received**: Fired after each WebDAV response
    - Contains: status_code, headers, body (XML), method used

### XML Body Construction

#### PROPFIND Example

**All properties:**

```xml
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:allprop/>
</D:propfind>
```

**Specific properties:**

```xml
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getcontentlength/>
    <D:getlastmodified/>
    <D:resourcetype/>
  </D:prop>
</D:propfind>
```

The LLM specifies which properties to request (or `null` for all), and we construct the XML automatically.

### Headers

WebDAV uses special HTTP headers:

- **Depth**: Controls recursion level for PROPFIND/COPY
    - `0`: Resource only
    - `1`: Resource + immediate children
    - `infinity`: All descendants
- **Destination**: Target path for COPY/MOVE
- **Overwrite**: T (true) or F (false) for COPY/MOVE
- **Lock-Token**: not sent. Nothing in `src/client/webdav/` sets this header; it is listed
  here only because LOCK/UNLOCK were once expected to be reachable.

## Implementation Details

### Custom HTTP Methods

reqwest doesn't natively support WebDAV methods, so we use:

```rust
let method = reqwest::Method::from_bytes(b"PROPFIND")?;
let request = http_client.request(method, url);
```

### XML Response Parsing

We **don't** parse XML in the client implementation. Instead:

1. Return raw XML body to LLM
2. LLM extracts information from XML (file names, sizes, types)
3. LLM decides next action based on XML content

This keeps the implementation simple and lets the LLM handle XML understanding.

### Authentication

WebDAV typically uses HTTP Basic Auth or Digest Auth. This client implements **Basic** only.

The `auth` startup parameter takes `username:password` and is resolved once at connect into
an `Authorization: Basic <base64(username:password)>` header, stored in `protocol_data` as
`startup_headers` and applied to every request by `perform_request`. Digest is **not**
supported — nothing challenges/responds, so a Digest-only server will keep answering 401.

## Limitations

1. **No XML Parsing**: We return raw XML to LLM, relying on LLM's XML understanding
2. **No locking at all**: LOCK and UNLOCK are not exposed as actions, so there is nothing
   to track. `perform_request` can build the methods; nothing can ask it to.
3. **No Versioning**: WebDAV versioning extensions (DeltaV) not supported
4. **No Access Control**: ACL methods not implemented
5. **No Quota Support**: QUOTA extension not implemented
6. **Digest auth**: Only Basic is implemented, via the `auth` startup parameter

## Example Prompts

### List Directory

```
Connect to http://webdav.example.com/dav and list all files in the /documents/ folder
```

LLM generates:

```json
{
  "type": "propfind",
  "path": "/documents/",
  "depth": "1"
}
```

### Upload File

```
Upload a file named hello.txt with content "Hello, WebDAV!" to /dav/files/
```

LLM generates:

```json
{
  "type": "put",
  "path": "/dav/files/hello.txt",
  "content": "Hello, WebDAV!",
  "content_type": "text/plain"
}
```

### Create Folder

```
Create a new folder named "projects" in /dav/
```

LLM generates:

```json
{
  "type": "mkcol",
  "path": "/dav/projects/"
}
```

### Copy File

```
Copy /dav/report.pdf to /dav/backup/report.pdf
```

LLM generates:

```json
{
  "type": "copy",
  "source": "/dav/report.pdf",
  "destination": "/dav/backup/report.pdf",
  "overwrite": true
}
```

## Testing Strategy

What exists today, in `tests/client/webdav/`:

- `e2e_test.rs` — `test_webdav_client_propfind` and `test_webdav_client_llm_controlled`. Both
  point the client at **NetGet's own WebDAV server**, so the peer is this repository's code:
  circular evidence, and the assertions are weak (one checks the client's output contains
  "WebDAV" or "connected" or "PROPFIND"; the other only that `client.protocol == "WebDAV"`).
  Pointing these at a third-party server — `wsgidav` was the plan and was never done — is what
  would make them evidence.
- `command_channel_test.rs` — the substantive one. A raw loopback HTTP stub asserts the exact
  `PROPFIND /dashboard-marker/` an injected action puts on the wire, and that an unknown action
  is `Rejected` and `disconnect` drops the handle. Zero LLM calls.

Target: **< 10 LLM calls** per test suite.

## Known Issues

1. **URL Encoding**: Paths with special characters may need encoding
2. **Namespaces**: XML namespaces assumed to be `DAV:` - custom namespaces not handled
3. **Error Handling**: HTTP errors returned as events, but no special WebDAV error parsing
4. **Chunked Uploads**: Large files uploaded in single request (no chunking)

## Future Enhancements

- Add Digest authentication (Basic already works via the `auth` startup parameter)
- Parse common XML responses (multistatus) to provide structured data
- Support WebDAV extensions (CalDAV, CardDAV)
- Implement chunked uploads for large files
- Add lock token management for exclusive access

## Injected actions (the dashboard's `[ send ]`)

The client registers a command channel and spawns `WebdavClient::command_loop` **before**
the `webdav_connected` LLM call, because a `*` -> manual rule can park that call for minutes
and the operator must still be able to reach the client.

The old `execute_webdav_action` is now `WebdavClient::apply_action`, taking an already
executed `ClientActionResult`; the connected-event path and the command loop both call it,
so an injected `propfind` builds the identical request (Depth/Destination/Overwrite headers,
PROPFIND XML body) the LLM path would.

`ClientSendOutcome` semantics:

| Outcome | When |
|---|---|
| `Executed { detail }` | The request ran to completion, e.g. `PROPFIND /files/ completed`. |
| `Rejected { error }` | `execute_action` refused the action (unknown verb, missing `path`). |
| `Disconnected` | `{"type":"disconnect"}`; status goes to Disconnected and the handle is dropped. |
| `Err(...)` | The HTTP request failed, or the result was one this client cannot apply. |

**`Sent { bytes_sent }` is never reported**: reqwest owns the socket. The request is awaited
before the outcome is returned, so `Executed` means it really completed and
`webdav_response_received` has already fired.
