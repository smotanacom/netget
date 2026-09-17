# NPM Registry Server Implementation

## Overview

NPM protocol implementation that acts as an NPM registry server, allowing the LLM to control package metadata, tarballs,
listings, and search results.

## Library Choices

### HTTP Server: hyper (v1.5)

- **Why**: Standard HTTP/1 server library used across NetGet for HTTP-based protocols
- **Compliance**: Full HTTP/1.1 support
- **Maturity**: Production-ready, widely used
- **Control**: Complete request/response control with service_fn pattern

### Base64 Encoding: base64 (v0.22)

- **Why**: NPM tarballs are transferred as base64-encoded data in LLM responses
- **Usage**: LLM provides tarball data as base64 string, decoded before sending to client

### No NPM-specific libraries

- **Rationale**: NPM registry protocol is pure HTTP/JSON with no complex binary formats
- **Implementation**: Manual JSON formatting based on NPM registry API spec

## Architecture

### Server Structure

```
NPM Registry Endpoints:
├── GET /{package}              → Package metadata (package.json manifest)
├── GET /{package}/-/{tarball}  → Package tarball download (.tgz)
├── GET /-/all                  → List all packages
└── GET /-/v1/search?text=...   → Search packages
```

### Request Flow

1. Client makes HTTP GET request to NPM endpoint
2. Server parses request path and determines operation type
3. Event created with request details (method, path, query)
4. LLM called with event context and server instruction
5. LLM responds with appropriate NPM action
6. Server processes action and sends HTTP response

### LLM Integration

The LLM controls all NPM registry responses through 5 actions:

#### Sync Actions (Network Event Responses)

1. **npm_package_metadata** - Return package metadata
    - Parameters: `metadata` (JSON object with package.json structure)
    - Response: HTTP 200 with JSON metadata
    - Used for: `GET /{package}` requests

2. **npm_package_tarball** - Return package tarball
    - Parameters: `tarball_data` (base64-encoded .tgz file)
    - Response: HTTP 200 with `application/octet-stream` content
    - Used for: `GET /{package}/-/{tarball}.tgz` requests
    - **This is the one npm response the model cannot author.** A .tgz is
      binary, so it can only be *relayed* from bytes the operator supplied.
      Undecodable or empty `tarball_data` is now a 500 naming the decode
      failure; it used to be `unwrap_or_default()`, which served **HTTP 200
      with a zero-byte body** — `npm install` then failed inside tar
      extraction with nothing pointing back at the model's answer, and an
      empty package was indistinguishable from a real one. The action's own
      example made that likely rather than theoretical: it showed an elided
      `"H4sIAAAAAAAAA..."`, which does not decode. Both examples now carry a
      complete, verified .tgz, and the action tells the model to answer
      `npm_error` 404 when it has no tarball to serve.

3. **npm_package_list** - Return list of all packages
    - Parameters: `packages` (JSON object mapping package names to metadata)
    - Response: HTTP 200 with JSON package list
    - Used for: `GET /-/all` requests

4. **npm_package_search** - Return search results
    - Parameters: `results` (JSON object with objects array and total count)
    - Response: HTTP 200 with JSON search results
    - Used for: `GET /-/v1/search?text=...` requests

5. **npm_error** - Return error response
    - Parameters: `error` (message), `status_code` (optional, default 500)
    - Response: HTTP error with JSON error object
    - Used for: Error conditions (package not found, invalid request, etc.)

#### Async Actions

None - all operations are request/response driven.

### Event Types

1. **NPM_PACKAGE_REQUEST** - Client requests package metadata (`GET /{package}`)
2. **NPM_TARBALL_REQUEST** - Client requests package tarball (`GET /{package}/-/{tarball}`)
3. **NPM_LIST_REQUEST** - Client requests package listing (`GET /-/all`)
4. **NPM_SEARCH_REQUEST** - Client requests package search (`GET /-/v1/search`)

### Connection State

Each connection tracks:

- Connection ID, remote/local addresses
- Bytes sent/received, packets sent/received
- Last activity timestamp
- Connection status (Active/Closed)
- Recent NPM requests (path, method, timestamp)

## Logging Strategy

### TRACE Level

- Full request details (method, path, query, headers)
- Full response payloads for debugging

### DEBUG Level

- Request summaries (method, path, operation type)
- Response types (metadata, tarball, list, search)
- Tarball sizes
- LLM call initiation

### INFO Level

- Server startup (listening address)
- Connection accepted/closed
- Major events (new client connection)

### WARN Level

- Unexpected HTTP methods (POST, PUT, DELETE)
- Malformed requests

### ERROR Level

- LLM call failures
- Server accept() failures
- Unexpected action results
- A response action missing the field it needs, or `tarball_data` that does
  not decode. Each answers HTTP 500 naming the problem. These branches used to
  `.unwrap()`, which panicked the per-connection tokio task — the panic is
  swallowed by `tokio::spawn`, so the server kept reporting `Running` while the
  client hung until its own timeout, with nothing in the log connecting the two.

## Example Prompts

### Basic Package Serving

```
Start an NPM registry on port 4873 that serves the "express" package version 4.18.2
```

### Virtual Repository

```
Create an NPM registry on port 4873 with these packages:
- express 4.18.2
- lodash 4.17.21
- react 18.2.0
```

### Fictional Packages

```
Be a surreal NPM registry on port 4873 that serves absurd packages like "coffee-script-coffee"
and "left-pad-right" with humorous descriptions
```

### Search Server

```
NPM registry on port 4873 that supports searching. When searched for "http", return
packages like express, axios, request
```

## Limitations

1. **No Publishing**: Only supports GET requests, no package publishing (PUT/POST)
2. **No Authentication**: No support for scoped packages or authentication
3. **Virtual Storage**: No actual file storage - LLM maintains "virtual" packages through conversation
4. **Tarball Generation**: LLM must provide pre-generated base64-encoded tarballs
5. **Limited Metadata**: LLM must manually construct NPM-compliant package.json structures
6. **No Dependency Resolution**: No automatic dependency graph calculation
7. **No Versioning**: LLM must manually handle version negotiation
8. **No npm-specific headers**: No support for npm-specific HTTP headers (X-npm-session-id, etc.)

## Protocol Compliance

- ✅ Package metadata endpoint (`GET /{package}`)
- ✅ Tarball download endpoint (`GET /{package}/-/{tarball}.tgz`)
- ✅ Package listing endpoint (`GET /-/all`)
- ✅ Search endpoint (`GET /-/v1/search`)
- ❌ Package publishing (`PUT /{package}`)
- ❌ User authentication
- ❌ Scoped packages (`@scope/package`)
- ❌ Deprecation warnings
- ❌ npm CLI compatibility headers

## Testing Approach

E2E tests use real `npm` CLI client to:

1. Configure custom registry URL (`npm config set registry http://localhost:{port}`)
2. Request package metadata (`npm view <package>`)
3. Install packages (`npm install <package>`)
4. Search packages (`npm search <query>`)

LLM responds with realistic NPM registry JSON structures and tarball data.

## Security Considerations

- No input validation on package names (LLM decides what's valid)
- No rate limiting
- No abuse prevention
- Intended for local testing/experimentation only
- Should not be exposed to public internet

## Performance Notes

- Each request requires LLM call (unless scripting mode used)
- Large tarballs require base64 encoding/decoding overhead
- No caching - every request processed fresh
- Connection pooling handled by HTTP/1.1 keep-alive

## Failure behaviour

Every terminal outcome of an NPM request is logged with a stable `decision=` token. The stakes
here are higher than in a protocol whose peer is a human: **the npm CLI acts on a 200**, taking
the body as a packument or a tarball and unpacking it. So the invariant this section exists to
record is that no failure path can produce one.

All of these are decided in `src/server/npm/mod.rs`. Nothing is delegated to
`src/server/http_common/handler.rs`, so the status code and the token are set in the same
place.

| Outcome | On the wire | Log |
|---|---|---|
| Model answered `npm_package_metadata` / `_tarball` / `_list` / `_search` | 200 with that body | INFO `decision=model_answer` |
| Model answered `npm_error` | the status code the model chose (4xx/5xx) with its message | INFO `decision=model_reject` |
| Model answered with no usable action | 500 `{"error":"No NPM action returned"}` | WARN `decision=model_silent` |
| Model answered but every action failed | the same 500 | ERROR `decision=fail_closed_bad_action` |
| Response action missing its field, or `tarball_data` that does not decode | 500 with the `WireFailure` category | ERROR `decision=fail_closed_bad_action` |
| Backend failed / retries exhausted | 500 with the `WireFailure` category | ERROR `decision=fail_closed_llm_error` |
| Backend saturated | 500 with `backend at capacity, retry later` | ERROR `decision=fail_closed_llm_overloaded` |
| Request was not a GET | 405 | WARN `decision=protocol_error` |
| The server row is gone from `AppState` | 500 | ERROR `decision=fail_closed_server_missing` |

`fail_closed_server_missing` is npm-specific and is named here because it is not a model
outcome at all: nothing was asked, because there was nothing to ask.

**No fail-open was found.** The dangerous shape — a backend outage or a silent model answering
200 with an empty or synthesised packument — cannot occur: both paths are 500, and the
zero-byte-tarball case was already closed (an `unwrap_or_default()` that served 200 with an
empty body). Two observations from the same read, neither a fail-open:

- **Overload and outage share the 500.** `http` distinguishes them (503 + `Retry-After` vs
  500) and npm does not; only the token separates them. Changing the status is a wire change
  and was left alone.
- **`"metadata": null` is served as a 200 whose body is `null`.** `data.get("metadata")`
  accepts a JSON null, so the model can produce a 200 npm cannot use. That is the model's own
  answer rather than a failure default, so it is tagged `model_answer` honestly, but the field
  is worth a presence check.

Tested by `tests/server/npm/decision_tag_test.rs`.

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a connection task and an `AppState` entry forever,
pre-authentication, on a server that would happily accept a hundred more. It now declares both
halves; the constants and the reasoning live beside them in `src/server/npm/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 30s | HTTP is client-speaks-first, so a peer that has connected and sent no byte has begun no request. 30s sits between nginx's `client_header_timeout` default of 60s and Apache's `RequestReadTimeout header=20`. Enforced with `TcpStream::peek` before the socket reaches hyper, so the request line is still there for hyper afterwards. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 300s | npm's own `fetch-timeout` default is 300000 ms (`npm config get fetch-timeout`): the client gives up on a silent server at exactly this point, so closing a connection that has been silent for as long can never be the side that breaks a working install. `npm install` downloads, then unpacks locally — only the gap between two requests is measured. |
| `MAX_CONNECTIONS` | 256 | Each admitted connection may hold one whole in-memory response body (a packument or a tarball), so the cap turns that per-connection bound into a total one. Refusal: **HTTP/1.1 `503 Service Unavailable` with `Retry-After: 5`** — npm retries a 503 with backoff (`fetch-retries`, 2 by default) rather than failing outright. Nothing of netget's reaches the wire; the reason is logged under `decision=fail_closed_connection_cap`. |

**The idle bound is a watchdog, not a read deadline, and that is not a stylistic choice.** hyper
owns every read once `serve_connection` starts and keeps polling the connection for new frames
*while a request is being answered*, so a deadline on those reads would fire in the middle of an
LLM round-trip. It watches `ConnectionActivity` instead, which reports a connection with work in
flight as not idle at all — so the model round-trip, and a `manual` rule parking an event for a
human (`src/state/intercepts.rs`, 300s by default), sit outside every deadline by construction.
That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse: TFTP evicted
live transfers because "idle" was measured wrongly.

**One residual, stated rather than hidden.** The first-byte bound is discharged the moment any
byte arrives, so a peer that sends one byte of a request line and then stops is bounded by the
*idle* deadline (300s) rather than by the 30s one. hyper's own `header_read_timeout` cannot
express the pair: it re-arms whenever hyper starts reading a head, an idle keep-alive connection
included, so setting it to 30s would close every pooled client between requests — collapsing the
two bounds onto one number, which is exactly what the pair exists to avoid.

`tests/server/npm/connection_bounds_test.rs` drives all three from the wire, over
`tests/helpers/http_bounds.rs`. Each was verified by **removing** the bound and watching the test
fail: with the `peek` deadline gone the silent peer still held the socket at 60s; with the idle
bound collapsed to 30s an answered connection was closed after 38s of silence; with the permit
released early the connection past the cap was served instead of refused. The real-client evidence this protocol's rating rests on — `npm install` against the real CLI (`test_npm_with_real_cli`) — still passes.
