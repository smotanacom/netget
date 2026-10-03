# WebDAV Client Test Strategy

## Overview

Tests for the WebDAV client protocol verify LLM-controlled WebDAV operations including PROPFIND, MKCOL, COPY, MOVE,
DELETE, PUT, and GET.

## Test Approach

**Black-box E2E testing** - Tests spawn actual NetGet binary and verify client behavior through observable outputs.

### Test Categories

1. **Basic Connectivity**
    - Test client initialization
    - Verify protocol registration
    - Check connection establishment

2. **WebDAV Operations**
    - PROPFIND (directory listing)
    - MKCOL (create collection/directory)
    - PUT (upload file)
    - GET (download file)
    - COPY (copy resource)
    - MOVE (move/rename resource)
    - DELETE (delete resource)

3. **LLM Control**
    - Verify LLM interprets WebDAV instructions
    - Check LLM constructs proper WebDAV requests
    - Validate LLM processes XML responses

## LLM Call Budget

**Target: < 10 LLM calls per test suite**

Current test plan:

1. `test_webdav_client_propfind`: 2 LLM calls (server + client)
2. `test_webdav_client_llm_controlled`: 2 LLM calls (server + client)

**Total: 4 LLM calls** ✅ Well under budget

### Efficiency Strategies

- **Reuse servers**: Start one server, multiple client connections
- **Simple instructions**: Minimal LLM decision-making
- **Test grouping**: Bundle related operations in single test

## Expected Runtime

- **Per test**: 1-2 seconds
- **Full suite**: 5-10 seconds

Fast execution due to:

- Local connections (127.0.0.1)
- Simple WebDAV operations
- Minimal LLM calls

## Test Server

Tests use NetGet's built-in WebDAV server:

- Listens on `{AVAILABLE_PORT}` (dynamically allocated)
- Responds to WebDAV methods
- LLM-controlled responses

## Test Execution

```bash
# Run WebDAV client E2E tests
./cargo-isolated.sh test --no-default-features --features webdav --test client::webdav::e2e_test

# With ollama lock (for concurrent tests)
./cargo-isolated.sh test --no-default-features --features webdav --test client::webdav::e2e_test -- --test-threads=1
```

## Known Issues

1. **No Real WebDAV Server**: Tests currently use NetGet's WebDAV server, not a production server like Apache/Nginx
2. **Limited XML Parsing**: Client returns raw XML to LLM, no structured parsing
3. **No Authentication**: Tests don't verify Basic/Digest auth support
4. **No Advanced Features**: No testing of LOCK/UNLOCK, versioning, or properties

## Future Enhancements

- Test against real WebDAV server (e.g., wsgidav Python package)
- Add authentication tests
- Test LOCK/UNLOCK operations
- Test PROPPATCH (modify properties)
- Test error handling (404, 409, etc.)
- Test XML namespace handling
- Test depth headers (0, 1, infinity)

## Test Validation

Tests validate:

- ✅ Client protocol is "WebDAV"
- ✅ Client output contains WebDAV-related keywords
- ✅ Client connects successfully
- ✅ LLM generates appropriate WebDAV actions

Tests do NOT validate:

- ❌ Exact XML structure (LLM-dependent)
- ❌ Server response correctness (tested in server tests)
- ❌ Complex WebDAV features (versioning, ACL, etc.)
