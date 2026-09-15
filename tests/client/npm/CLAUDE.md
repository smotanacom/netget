# NPM Registry Client E2E Tests

## Test Strategy

### Approach

Black-box E2E tests using the **public NPM registry** (registry.npmjs.org). Tests verify:

1. Connection initialization
2. Package information retrieval
3. Package search functionality
4. Tarball download
5. Scoped package handling

### Test Environment

- **Registry**: Public NPM registry (registry.npmjs.org)
- **Network**: Requires internet connection
- **LLM**: Requires Ollama running locally (http://localhost:11434)
- **Filesystem**: Tests write to temp directory for tarball downloads

### LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Actual**: ~5-6 LLM calls

#### Per-Test LLM Calls

1. **test_npm_client_get_package_info** (2 LLM calls)
    - Connect event (npm_connected)
    - Package info received event (npm_package_info_received)

2. **test_npm_client_search_packages** (2 LLM calls)
    - Connect event
    - Search results received event (npm_search_results_received)

3. **test_npm_client_download_tarball** (1 LLM call)
    - Connect event (download doesn't trigger LLM, it's a direct operation)

4. **test_npm_client_scoped_package** (2 LLM calls)
    - Connect event
    - Package info received event

**Total**: ~7 LLM calls

### Budget Rationale

- NPM client is stateless (no persistent connection)
- Each test establishes a fresh connection
- LLM is called only for event processing (connect, data received)
- Download operations are direct HTTP requests (no LLM needed)

## Expected Runtime

- **Per Test**: 10-20 seconds (depends on network and LLM speed)
- **Full Suite**: ~60 seconds
- **Network Latency**: NPM registry is fast (< 1s per request)
- **LLM Latency**: Ollama local inference (2-5s per call)

## Test Cases

### 1. Get Package Information

**Purpose**: Verify client can retrieve package metadata

**Steps**:

1. Connect to NPM registry
2. Request "lodash" package info
3. LLM processes package data (version, description, dist)

**Expected**:

- Client connects successfully
- Package info retrieved with valid structure
- LLM can analyze package versions and metadata

### 2. Search Packages

**Purpose**: Verify client can search NPM registry

**Steps**:

1. Connect to NPM registry
2. Search for "http server" packages
3. LLM processes search results

**Expected**:

- Client connects successfully
- Search returns multiple results
- LLM can analyze package list

### 3. Download Tarball

**Purpose**: Verify client can download package tarballs

**Steps**:

1. Connect to NPM registry
2. Download "lodash" tarball to temp directory
3. Verify file exists and has content

**Expected**:

- Client connects successfully
- Tarball downloads to specified path
- File is valid .tgz archive (> 0 bytes)

### 4. Scoped Package

**Purpose**: Verify client handles scoped packages (@scope/package)

**Steps**:

1. Connect to NPM registry
2. Request "@types/node" package info
3. LLM processes scoped package data

**Expected**:

- Client properly encodes package name (@types%2fnode)
- Package info retrieved successfully
- Scoped package URL handling works correctly

## Known Issues

### 1. Network Dependency

**Issue**: Tests require internet access
**Impact**: CI/CD must have network access
**Mitigation**: Use `#[ignore]` attribute (tests run only when explicitly requested)

### 2. LLM Dependency

**Issue**: Tests require Ollama running locally
**Impact**: Cannot run in environments without Ollama
**Mitigation**: Use `#[ignore]` attribute, document in test docstring

### 3. Registry Availability

**Issue**: Tests fail if NPM registry is down or rate-limited
**Impact**: False negatives in CI/CD
**Mitigation**:

- Use well-known stable packages (lodash, @types/node)
- Set reasonable timeouts (30s for queries, 120s for downloads)
- Retry logic in HTTP client (via reqwest)

### 4. Package Availability

**Issue**: Tests assume specific packages exist ("lodash", "@types/node")
**Impact**: Tests could break if packages are unpublished
**Mitigation**: Use extremely popular packages with millions of downloads

## Running Tests

### Run All NPM Client Tests

```bash
./cargo-isolated.sh test --no-default-features --features npm --test client::npm::e2e_test -- --ignored
```

### Run Specific Test

```bash
./cargo-isolated.sh test --no-default-features --features npm --test client::npm::e2e_test test_npm_client_get_package_info -- --ignored
```

### Prerequisites

1. Ollama running: `ollama serve`
2. Internet connection
3. npm feature enabled

## Test Data

### Packages Used

1. **lodash** (v4.17.21+)
    - Extremely popular utility library
    - Stable API, rarely changes
    - Small tarball (~100KB)

2. **@types/node** (latest)
    - TypeScript type definitions
    - Popular scoped package
    - Good test for URL encoding

3. **Search Query**: "http server"
    - Returns popular packages (express, koa, fastify)
    - Good variety for LLM analysis

## Test Maintenance

### When to Update

1. **Breaking NPM API Changes**: Update endpoints if NPM changes API
2. **Package Deprecation**: Replace test packages if unpublished
3. **LLM Model Changes**: Adjust expectations if model behavior changes
4. **Rate Limiting**: Add backoff/retry if registry imposes limits

### Monitoring

- Check test pass rate in CI/CD
- Monitor NPM registry availability
- Track LLM call count (should remain < 10)
- Verify download file sizes (should be reasonable, not empty)

## Future Enhancements

1. **Mock NPM Registry**: Add local mock server for CI/CD
2. **Snapshot Testing**: Verify exact package structure
3. **Authentication Tests**: Test private package access (when implemented)
4. **Rate Limit Handling**: Test client behavior under rate limits
5. **Dependency Analysis**: Test following dependency chains
6. **Publish Test**: Test package publishing (when implemented)

## `command_channel_test.rs` — the dashboard's `[ send ]`

One test, **zero LLM calls**. The client's LLM points at `http://127.0.0.1:1`, so the
connected-event call (where there is one) and every response-event call fail immediately;
tolerating that failure is part of what the test verifies.

The registry is a throwaway `TcpListener` on 127.0.0.1 that speaks just enough HTTP/1.1 and records the request lines it saw. **Nothing contacts registry.npmjs.org** — that is a hard project rule, and it is why `search_packages` and `download_tarball` are not exercised here: NPM's search endpoint is hardcoded to the public registry, and the tarball path follows a `dist.tarball` URL.

What it asserts, in order:

1. `wait_for_client_handle` — the regression guard for "register the channel before the
   connected-event LLM call". Without it, a manual routing rule would leave `[ send ]` dead
   for the length of the park.
2. The injected action returns `Executed`, **not** `Sent` — `reqwest` reports no wire byte count, so a number would be invented.
3. The stub registry really saw `GET /express`. Because `apply_action` awaits the request, the outcome cannot arrive before the bytes did.
4. The client's access log gained an `injected_action` entry, so injected traffic shows up in
   the request pane exactly like LLM-produced traffic.
5. An unknown verb comes back `Rejected`, not silently swallowed.
6. `disconnect` returns `Disconnected`, the command loop exits, the handle is dropped and the
   client's status becomes `Disconnected`.

## `registry_target_test.rs` — where the traffic goes, and what is refused

Five tests, **zero LLM calls** (a `*` rule answers the connected event with nothing) and
**zero network traffic**: the NPM client issues no request at connect, and every case removes
its client before returning.

`resolve_registry_url` used to end in `return "https://registry.npmjs.org".to_string()` for an
empty `remote_addr`, logging an INFO as it went — "a client that loses its target must fail,
never fall back to the real service", milder than the DynamoDB case only because nothing here
is signed. It now returns `Result` and refuses.

Driven through `ClientForm::create`, so what is asserted is the `registry_url` the client
recorded for itself rather than a helper's return value:

- a scheme-qualified address is used exactly as given (trailing `/` trimmed);
- a bare host gets `https://` rather than being discarded;
- **naming the public registry explicitly is still honoured** — the control. A guard that
  refused everything would pass the two refusal tests below and break the protocol's own
  startup example;
- an empty address refuses, and the error names `remote_addr`;
- `"npm"` — the protocol's own name, which reaches the resolver when a caller fills
  `remote_addr` with the thing it is starting — refuses too.

Verified by restoring the fallback: the two refusal tests fail, the three others pass either
way, which is what makes them controls rather than decoration.

