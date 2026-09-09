# WebDAV Protocol Implementation

**Status**: `DevelopmentState::Beta` — exercised against `reqwest_dav`, a real and independent
WebDAV client, in a test that is neither `#[ignore]`d nor gated on an external binary. Not
Stable: the property model is fixed (no PROPPATCH dead-property storage), locks are accepted
and never enforced, and spec compliance has not been reviewed.

WebDAV (RFC 4918) over HTTP/1.1. `hyper` v1.0 carries the requests; this module answers the
DAV methods itself and generates the `DAV:multistatus` XML. **There is no filesystem** — the
LLM supplies every directory listing, every file body and the status of every write. Default
port in the examples is 8080; the caller picks it, and `privilege_requirement` is `None`
because nothing here binds a port below 1024 by default.

## The storage decision

The previous implementation handed every request to `dav_server::memfs::MemFs`, a real
read/write filesystem inside the process, and dropped the `OllamaClient` on startup so the
server instruction was read by nobody. Both are gone. Two replacements were possible:

1. **Implement `dav_server::fs::DavFileSystem` against the model.** Rejected. `dav-server`
   calls `metadata()` for the target and again for *every* directory entry, then `open()` and
   `read_bytes()` separately — so a model-backed `DavFileSystem` costs a round-trip per
   *property*, not per request, which no LLM call budget survives. It also cannot express the
   status codes WebDAV needs: `FsError` has no way to say 405, 409 or 507, and RFC 4918 makes
   the distinction between 201 and 204 on `PUT` load-bearing.
2. **Answer the verbs directly.** Chosen. One model round-trip per request, and the model
   picks the status code itself.

`dav-server` is therefore no longer used by this protocol, and has been removed from
`Cargo.toml` — the feature is now `webdav = ["dep:reqwest_dav"]`. `reqwest_dav` is still
needed: the WebDAV *client* protocol uses it, and so do these tests.

## What the model sees and controls

**Event**: `webdav_request`, one per request.

| Field | Notes |
|---|---|
| `method` | uppercased: `PROPFIND`, `GET`, `HEAD`, `PUT`, `DELETE`, `MKCOL`, `COPY`, `MOVE`, `PROPPATCH`, `POST` |
| `path` | percent-decoded, no query string. The model echoes this into `send_webdav_listing` |
| `depth` | `Depth` header, present only when sent: `"0"`, `"1"`, `"infinity"` |
| `destination` | `Destination` header of a COPY/MOVE, reduced to a path |
| `overwrite` | `Overwrite` header, `"T"` or `"F"` |
| `headers` | all request headers, names lowercased |
| `body` | request body decoded as UTF-8 **lossily** (PUT content, PROPFIND/PROPPATCH XML) |
| `body_bytes` | body size before decoding |
| `body_is_binary` | present and `true` only when the body was not valid UTF-8 |

**Actions** (all sync; there are no async actions — WebDAV is purely reactive):

| Action | Produces | Key parameters |
|---|---|---|
| `send_webdav_listing` | `207 Multi-Status` + generated XML | `path` (required, echo the event's), `entries[]` of `{name, is_collection, size, content_type, last_modified}`, `is_collection`, `size`, `content_type`, `last_modified` |
| `send_webdav_file` | `200` + text body | `content` (required), `content_type`, `status` |
| `send_webdav_status` | any status, optional body | `status` (required), `body`, `headers` |

All three are attached to `webdav_request` via `.with_actions(...)`, so the model actually sees
them — the defect that silently disabled seventeen protocols does not apply here, and
`tests/event_action_declarations_test.rs` covers it.

Nothing carries raw bytes or base64: a listing is structured entries and the XML is generated
from them, and file content is a string.

### Failure behavior — fails closed

Three distinguishable outcomes, deliberately:

- **Model refuses** → whatever status it chose (`403`, `404`, `412`, `423`…).
- **Model answers with no `send_webdav_*` action** → `503 Service Unavailable`, logged at
  ERROR. This is the fail-open trap OAuth2 fell into and is closed here: silence is never
  consent, and 503 is outside the set the model can pick, so a capture or an access log always
  tells the two apart.
- **LLM call fails** → `500`.

An out-of-range status becomes 500 rather than a panic, and a header hyper cannot represent
(CR/LF injection, i.e. response splitting) is dropped individually.

## Methods answered without the model

`OPTIONS`, `LOCK` and `UNLOCK` never raise `webdav_request`. They are handshakes with no
content to decide:

- `OPTIONS` → `200`, `Allow:` listing every method, `DAV: 1, 2`, `MS-Author-Via: DAV`.
- `LOCK` → `200` with a `lockdiscovery` body and a fresh `opaquelocktoken:` UUID. **The lock is
  synthetic and never enforced** — nothing in this protocol consults it. It exists because
  macOS Finder and the Windows WebDAV redirector refuse to write without one.
- `UNLOCK` → `204`.

Every response, including the model-driven ones, carries `DAV: 1, 2` — clients decide whether a
server speaks WebDAV from that header, not from the body.

## Architecture

```
TCP accept -> hyper http1::serve_connection -> service_fn
   -> extract_request (percent-decode path, buffer body)
   -> OPTIONS/LOCK/UNLOCK answered here
   -> Event::new(WEBDAV_REQUEST_EVENT) -> call_llm -> build_webdav_response
```

Handling-mode priority is the generic one: script handler → static handler → LLM. Script and
static handlers cost no LLM call, and static handlers can reach the event with
`{{event.path}}` interpolation, which is what makes a deterministic read-only share practical.

- The listener is bound **before** the accept task is spawned and the error propagates with
  `?`, so a bind failure reaches `server_startup` as `ServerStatus::Error` instead of a
  phantom `Running`. `local_addr()` is returned, so port 0 reports the port the kernel chose.
- The accept-loop `JoinHandle` is registered with `AppState::register_server_task()`, so
  `stop_server` releases the socket. Per-connection tasks are not tracked (project-wide
  limitation), so in-flight requests are not cancelled.
- One `ConnectionId` per TCP connection, closed when `serve_connection` returns.
  `bytes_*`/`packets_*`/`last_activity` are maintained per request on every exit path,
  including the ones that never reach the model. One "packet" is one HTTP message and the byte
  counts are message bodies only — hyper has parsed the request line and headers away by then.
  `last_activity` no longer risks eviction: `AppState::cleanup_old_connections` runs the 10s
  idle sweep only where `ProtocolMetadataV2::connectionless` is set, and WebDAV does not
  declare it — correctly, since clients hold keep-alive connections open between bursts.

## Limitations

- **Request bodies are capped at 8 MiB** (`MAX_REQUEST_BODY`). Over that the peer gets `413`
  with no model call — there is no content decision to make about a body the server declined
  to read. The cap is checked against the declared `Content-Length` before a byte is read, and
  enforced on the stream itself (`http_body_util::Limited`) for the chunked case. Before it
  existed the body was `.collect()`ed unbounded and then interpolated whole into the model's
  prompt, and WebDAV has no authentication step, so one unauthenticated request decided how
  much memory the process allocated.

- **No storage, by design.** A `PUT` is remembered only if the model chooses to remember it
  (server memory, or a script handler that keeps its own state). A `GET` after a `PUT` returns
  whatever the model says, which may not be what was written.
- Text content only. `send_webdav_file` writes UTF-8; images and archives cannot be served, and
  a non-UTF-8 `PUT` body reaches the model only as a lossy decoding.
- Request bodies are fully buffered with no size cap before the LLM call.
- Locks accepted, never enforced. No authentication. No TLS. No `Range` support (the model can
  set `206` and a partial body, but nothing parses the `Range` header for it).
- `PROPPATCH` can be answered but no dead property is stored; `PROPFIND` returns a fixed
  property set (`displayname`, `getlastmodified`, `resourcetype`, `getcontentlength`,
  `getcontenttype`) regardless of what the client's `<D:prop>` asked for.
- No DAV versioning (RFC 3253), no `Depth: infinity` expansion beyond what the model lists.

## Testing

`tests/server/webdav/test.rs` — three mocked E2E tests driven by `reqwest_dav`, 9 LLM calls
total. See `tests/server/webdav/CLAUDE.md`.

```bash
./cargo-isolated.sh test --no-default-features --features webdav \
    --test server -- --test-threads=100 webdav
```

## Example prompts

```
listen on port 8080 using webdav stack
The share root holds a documents folder and readme.txt containing "Hello from NetGet"
Accept PUT (201 for a new path, 204 to overwrite) and remember what was written
```

Deterministic read-only share with no LLM call per request:

```json
"event_handlers": [{
  "event_pattern": "webdav_request",
  "handler": { "type": "static", "actions": [
    { "type": "send_webdav_status", "status": 404, "body": "Not Found" }
  ]}
}]
```

## References

- [RFC 4918: WebDAV](https://tools.ietf.org/html/rfc4918)
- [reqwest_dav](https://docs.rs/reqwest_dav) — the client the tests drive
