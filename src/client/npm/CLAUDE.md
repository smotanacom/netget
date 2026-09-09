# NPM Registry Client Implementation

## Overview

The NPM Registry client allows LLM-controlled interaction with the NPM package registry (registry.npmjs.org or custom
registries). It provides package search, metadata retrieval, and tarball download capabilities.

## Library Choices

### Primary Library: `reqwest`

- **Why**: Industry-standard Rust HTTP client with excellent async support
- **Features**: Automatic TLS, timeouts, user-agent handling
- **Used for**: All HTTP interactions with NPM registry

### Secondary Library: `urlencoding`

- **Why**: Proper URL encoding for package names (especially scoped packages like @types/node)
- **Features**: RFC 3986 compliant encoding
- **Used for**: Encoding package names in URLs

## Architecture

### Connection Model

Unlike TCP-based protocols, NPM client is **stateless HTTP-based**:

- No persistent connection to maintain
- Each action triggers independent HTTP request(s)
- "Connection" is logical - represents client initialization state

### Registry URL

Default: `https://registry.npmjs.org`

Can be customized for:

- Private NPM registries
- Mirrors (e.g., Verdaccio, Nexus)
- Enterprise registries

### Package Name Encoding

Special handling for scoped packages:

```
@types/node → @types%2fnode
```

## LLM Integration

### Events

#### 1. `npm_connected`

**Trigger**: Client initialization
**Parameters**:

- `registry_url`: Registry URL being used

**LLM Decision**: Begin with package query or search

#### 2. `npm_package_info_received`

**Trigger**: Package metadata received
**Parameters**:

- `package_name`: Package name
- `version`: Version retrieved (e.g., "4.18.2")
- `description`: Package description
- `versions`: Array of all available versions
- `dist`: Distribution metadata (tarball URL, shasum)

**LLM Decision**:

- Query dependencies
- Download specific version
- Compare with other packages
- Search for related packages

#### 3. `npm_search_results_received`

**Trigger**: Search results received
**Parameters**:

- `query`: Search query used
- `results`: Array of packages (name, version, description)
- `total`: Total number of matches

**LLM Decision**:

- Select package to investigate
- Refine search
- Download specific package

### Actions

#### Async Actions (User-Triggered)

1. **`get_package_info`**
    - Query package metadata
    - Supports scoped packages
    - Can request specific version or "latest"

2. **`search_packages`**
    - Full-text search across NPM registry
    - Limit results (default: 20)
    - Returns package names, versions, descriptions

3. **`download_tarball`**
    - Fetch the .tgz and report its size plus the `integrity`/`shasum` the registry
      advertised for it
    - Automatically resolves the tarball URL from the packument
    - **Writes nothing to disk, and takes no path.** It used to end in
      `tokio::fs::write(&output_path, bytes)` with `output_path` taken verbatim from
      the model's action — an arbitrary-file-write driven by LLM output, and a straight
      violation of the rule that a protocol implements no storage. A model that merely
      misunderstood the parameter could have overwritten `~/.ssh/authorized_keys`. The
      parameter is gone from the action, not just from the executor
    - The download is capped at `MAX_TARBALL_BYTES` (64 MiB) and streamed, because the
      tarball URL comes out of the registry's own JSON and its size is therefore chosen
      by whatever this client was pointed at

4. **`disconnect`**
    - Cleanup client state

#### Sync Actions (Response-Triggered)

1. **`get_package_info`**
    - Query related packages (e.g., dependencies)

2. **`search_packages`**
    - Search based on received metadata

### Action Flow Example

```
User: "Find http server packages for Node.js"

1. LLM calls: search_packages("http server", 10)
2. Event: npm_search_results_received
   - Results: express, koa, fastify, hapi, ...
3. LLM calls: get_package_info("express", "latest")
4. Event: npm_package_info_received
   - Version: 4.18.2
   - Dist: { tarball: "https://...", shasum: "..." }
5. LLM calls: download_tarball("express", "4.18.2")
6. Download completes; the client reports the byte count and the advertised integrity
```

## NPM Registry API

### Endpoints Used

#### 1. Package Metadata

```
GET https://registry.npmjs.org/{package}
GET https://registry.npmjs.org/{package}/{version}
```

Returns:

- Package description
- All versions
- Distribution metadata (tarball URLs)
- Dependencies
- Keywords, license, etc.

#### 2. Search

```
GET https://registry.npmjs.org/-/v1/search?text={query}&size={limit}
```

Returns:

- Array of matching packages
- Total count
- Package metadata (name, version, description)

#### 3. Tarball Download

```
GET https://registry.npmjs.org/{package}/-/{package}-{version}.tgz
```

Returns: Binary .tgz file

### Response Formats

All metadata endpoints return JSON. Example:

```json
{
  "name": "express",
  "description": "Fast, unopinionated, minimalist web framework",
  "dist-tags": {
    "latest": "4.18.2"
  },
  "versions": {
    "4.18.2": {
      "name": "express",
      "version": "4.18.2",
      "dist": {
        "tarball": "https://registry.npmjs.org/express/-/express-4.18.2.tgz",
        "shasum": "..."
      }
    }
  }
}
```

## State Management

### Protocol Data Fields

- `npm_client`: "initialized"
- `registry_url`: Registry URL (default: https://registry.npmjs.org)

### Memory

LLM can track:

- Previously queried packages
- Search history
- Download decisions

## Logging Strategy

### Dual Logging

All operations logged via:

1. **tracing macros**: `info!`, `error!` → `netget.log`
2. **status_tx**: User-visible messages → TUI

### Log Levels

- **INFO**: Client lifecycle, API requests, successful operations
- **ERROR**: API failures, network errors, parse errors
- **DEBUG**: (not used - simple protocol)

### Example Logs

```
[INFO] NPM client 1 initialized for https://registry.npmjs.org
[INFO] NPM client 1 getting package info: express (latest)
[INFO] NPM client 1 received package info for express
[INFO] NPM client 1 downloading tarball for lodash (4.17.21)
[INFO] NPM client 1 downloading from: https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz
[INFO] NPM client 1 downloaded tarball to: ./lodash.tgz
[ERROR] NPM client 1 request failed: 404 Not Found
```

## Limitations

### 1. Authentication Not Implemented

- Cannot publish packages
- Cannot access private packages
- Cannot use authenticated registries

**Future**: Add NPM token support via headers

### 2. No Package Installation

- Only downloads tarballs
- Does not extract or install packages
- Does not resolve dependencies

**Workaround**: LLM can query dependency tree and download individually

### 3. No Rate Limiting

- NPM registry has rate limits (anonymous: ~300 req/15min)
- No built-in rate limiting or backoff
- Excessive requests may get throttled

**Mitigation**: LLM should batch queries intelligently

### 4. Search API Limitations

- Limited to text search (no filters)
- Cannot search by author, keywords only
- Results capped at 250 (NPM API limit)

### 5. Stateless Design

- Each action is independent
- No connection pooling
- No HTTP keep-alive optimization

**Trade-off**: Simplicity over performance (acceptable for LLM use case)

## Error Handling

### HTTP Errors

- **404 Not Found**: Package or version doesn't exist
- **5xx Server Errors**: NPM registry issues
- **Timeout**: Network or registry slow (30s default)

All errors:

1. Logged via `error!` macro
2. Sent to status_tx for user visibility
3. Returned as `Err(...)` to stop action execution

### Parse Errors

JSON parse failures indicate:

- Unexpected API response format
- Malformed data

Handled with `context()` to provide useful error messages.

## Testing Strategy

See `tests/client/npm/CLAUDE.md` for E2E testing approach.

## Example Prompts

### Basic Package Query

```
"Get information about the express package"
```

### Search and Download

```
"Search for lodash packages, then download the latest version to ./lodash.tgz"
```

### Dependency Analysis

```
"Get information about express, then query all its dependencies"
```

### Version Comparison

```
"Compare versions 4.17.21 and 4.18.0 of lodash"
```

## Implementation Notes

### Scoped Packages

Handle `@scope/package` format:

```rust
let encoded_name = package_name.replace("/", "%2f");
// @types/node → @types%2fnode
```

### Version Resolution

"latest" → Query dist-tags.latest from package metadata

Specific version → Query versions[version] from metadata

### User-Agent

Set to `NetGet NPM Client/1.0` for:

- Registry analytics
- Issue debugging
- Good citizenship

## Future Enhancements

1. **Authentication**: NPM token support for private packages
2. **Publishing**: `npm publish` equivalent
3. **Dependency Resolution**: Automatic tree walking
4. **Package Installation**: Extract and setup node_modules
5. **Registry Configuration**: .npmrc file support
6. **Cache**: Local metadata caching for repeated queries
7. **Rate Limiting**: Automatic backoff and retry
8. **Batch Operations**: Query multiple packages in parallel

## Injected commands (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running NPM client and gets back a truthful `ClientSendOutcome`. See `src/client/command_support.rs` and `tests/client/npm/command_channel_test.rs`.

- `command_support::register_command_channel` runs **before** anything that can block for a
  human (NPM raises no connected event today, so the channel is registered as soon as the client is marked `Connected`), so `[ send ]` works even while an event is parked on a manual routing
  rule. This is the whole point of the feature: registering late means the rail reads
  "no command channel" for the length of the park.
- The command loop **replaced the old 5-second `get_client().is_none()` idle poll**. When the
  client is removed its handle is dropped, the channel closes, `recv()` returns `None` and the
  loop exits at once — the poll was strictly slower at the same job. The loop is registered
  with `register_client_task`, and it drops the handle on every exit path.
- One `apply_action` is the only place actions become traffic, shared by the command loop and any future LLM dispatch. There is
  no second wire path for injected actions to drift from.

### Outcome semantics — what `[ send ]` reports, and why

| Outcome | When |
|---|---|
| `Sent { bytes_sent }` | **Never.** `reqwest` does not report how many bytes reached the wire, and inventing a number would be a lie |
| `Executed { detail }` | The operation ran to completion; `detail` names it (`get_package_info express@latest completed ...`) |
| `Rejected { error }` | `execute_action` refused the JSON (unknown verb, missing field) |
| `Disconnected` | `disconnect`; the loop ends, the handle is dropped and the client goes to `Disconnected` |
| `Err(...)` | The request itself failed (transport error, non-2xx). The caller sees the error rather than a false success |

**The known cost of awaiting.** `apply_action` awaits the whole operation, *including the
response event it raises*. That is what makes the outcome truthful, but it also means the
command loop is busy until that event has been handled — with a `*` -> manual rule, until the
human answers it or the intercept times out (default 300s). Injected sends queue behind it on
the bounded channel, which surfaces as "client busy" backpressure. `send_to_client`'s own
timeout protects the caller either way.

**This paragraph used to say the opposite of what the code does, and in the dangerous direction** — it claimed `get_package_info`/`search_packages` *discard* the model's actions (`actions: _`), that `npm_connected` is never raised, and that search hardcodes `https://registry.npmjs.org/-/v1/search`. All three had been fixed; a reader trusting this file would have re-fixed a solved bug. `run_follow_ups` executes what the model answers, `connect_with_llm_actions` raises `npm_connected`, and search uses the configured `registry_url`.

**What is true now.** `run_follow_ups` bounds the chain structurally at one turn: it routes through the non-notifying `perform_*` helpers, which raise no event, so a model cannot drive an unbounded action -> event -> action loop the way Maven's shape could. It handles all four verbs the model can see — `npm_download_tarball` used to fall through to the catch-all and be logged as having "no non-notifying path", which was simply untrue — and a model answering `{"type": "disconnect"}` now actually disconnects instead of being filtered out before the match.

**Startup parameters.** `registry_url` is declared in `get_startup_parameters()` **and read**: `connect()` prefers it over `ctx.remote_addr`. It was declared and read by nothing for as long as this client has existed — `connect()` forwarded `ctx.remote_addr` and dropped `ctx.startup_params` on the floor — so the advertised knob did nothing when turned.

**A scheme-less address is no longer discarded.** `remote_addr` without `http://`/`https://` used to be thrown away and silently replaced with `https://registry.npmjs.org`, so an operator who typed `127.0.0.1:8080` had their requests sent to the public registry with no warning — and this protocol's own startup example (`"remote_addr": "registry.npmjs.org"`) took exactly that branch. A host now gets the `https://` it was missing; only an empty address falls back, and it says so in the log.

**One `reqwest::Client`, built once, off the runtime.** Every request used to build a fresh one, and `connect()` built a further one into `_http_client` and dropped it immediately. Building a client is blocking — rustls setup plus the platform root store, which on macOS reads the keychain through Security.framework — so this was the systemic defect `CLAUDE.md` records as having stalled a whole client runtime, paid per request.
