# PyPI Client E2E Testing

## Test Strategy

### Black-Box Testing

All tests treat NetGet as a black-box system, spawning the actual binary and observing behavior through:

- Client output logs
- Protocol detection
- Error handling

### Real PyPI API

Tests use the real PyPI API (https://pypi.org) instead of mocking:

- **Pros**: Tests real-world behavior, network handling, API changes
- **Cons**: Requires network access, slower, potential for flakiness
- **Mitigation**: Use well-known stable packages (requests, flask, django)

### Test Packages

Selected packages for testing:

1. **requests** - Popular, stable, small metadata
2. **flask** - Web framework, moderate size
3. **django** - Large framework, extensive metadata
4. **Non-existent package** - Error handling test

## LLM Call Budget

Target: **< 5 LLM calls** total

### Call Breakdown

1. **Package Info Query** - 1 LLM call
    - Connect to PyPI
    - Execute get_package_info action
    - Process response

2. **List Files** - 1 LLM call
    - Connect to PyPI
    - Execute list_package_files action
    - Analyze file list

3. **Error Handling** - 1 LLM call
    - Connect to PyPI
    - Attempt invalid package query
    - Handle error response

4. **LLM Exploration** - 1 LLM call
    - Connect to PyPI
    - Follow instruction to explore package
    - Analyze metadata

**Total**: 4 LLM calls (within budget)

## Test Cases

### 1. Package Info Query (`test_pypi_get_package_info`)

**Purpose**: Verify client can fetch package metadata via JSON API

**Instruction**: "Connect to PyPI and get information about the 'requests' package. Show the package description and
latest version."

**Expected Behavior**:

- Client connects to https://pypi.org
- LLM receives pypi_connected event
- LLM executes get_package_info("requests")
- Client fetches https://pypi.org/pypi/requests/json
- LLM receives pypi_package_info_received event
- Output contains: "PyPI", "requests", or "package"

**Validation**:

- Output mentions PyPI protocol or package info
- No connection errors

**Runtime**: ~3 seconds

---

### 2. List Package Files (`test_pypi_list_package_files`)

**Purpose**: Verify client can list available distribution files

**Instruction**: "Connect to PyPI and list all available distribution files (wheels and source distributions) for the '
flask' package."

**Expected Behavior**:

- Client connects to PyPI
- LLM executes list_package_files("flask")
- Client fetches package JSON and extracts URLs
- LLM receives pypi_package_info_received with files list
- Output shows wheel (.whl) and/or source (.tar.gz) files

**Validation**:

- Protocol is "PyPI"
- Output mentions wheels, files, or tar.gz
- No errors

**Runtime**: ~3 seconds

---

### 3. Non-existent Package (`test_pypi_nonexistent_package`)

**Purpose**: Verify error handling for invalid package names

**Instruction**: "Connect to PyPI and try to get information about the package '
this-package-definitely-does-not-exist-12345678'."

**Expected Behavior**:

- Client connects to PyPI
- LLM attempts get_package_info for non-existent package
- PyPI API returns 404 Not Found
- Client returns error
- LLM receives error or handles gracefully
- Output shows error message

**Validation**:

- Output contains: "ERROR", "not found", "404", or "failed"
- Client doesn't crash

**Runtime**: ~3 seconds

---

### 4. LLM-Controlled Exploration (`test_pypi_llm_controlled_exploration`)

**Purpose**: Verify LLM can autonomously explore packages

**Instruction**: "Connect to PyPI and explore the 'django' package. Look at its metadata and determine if it's a web
framework."

**Expected Behavior**:

- Client connects to PyPI
- LLM decides to query django package info
- LLM analyzes metadata (description, keywords, classifiers)
- LLM determines package type
- Output shows exploration results

**Validation**:

- Protocol is "PyPI"
- Output mentions "django", "package", or "info"
- LLM demonstrates autonomous decision-making

**Runtime**: ~3 seconds

---

## Known Issues

### Network Dependency

- Tests require internet access to pypi.org
- May fail if PyPI is down or slow
- **Mitigation**: Use well-known stable packages

### API Rate Limiting

- PyPI has rate limits for anonymous requests
- Running many tests quickly may trigger limits
- **Mitigation**: Use delays between tests, < 5 total calls

### Response Size

- Large packages (numpy, tensorflow) have large JSON responses
- May cause timeouts or memory issues
- **Mitigation**: Use smaller packages for basic tests

### Search API Deprecation

- PyPI's XML-RPC search API is deprecated
- search_packages action returns notice instead of results
- **Mitigation**: Tests focus on get_package_info, not search

## Timeout Configuration

All tests use appropriate timeouts:

- **Connection timeout**: 30 seconds (reqwest default)
- **Test timeout**: 5 seconds per test
- **Sleep duration**: 3 seconds for LLM processing

## Debugging

### Common Failures

1. **Network timeout**
    - Symptom: Test hangs or times out
    - Fix: Check internet connection, increase sleep duration

2. **Package not found**
    - Symptom: 404 errors for test packages
    - Fix: Use different stable package

3. **LLM didn't trigger action**
    - Symptom: No API request made
    - Fix: Check LLM instruction clarity, verify protocol parsing

### Output Inspection

Tests use `client.get_output().await` to inspect:

- Protocol detection ("PyPI")
- Connection messages
- Error messages
- Package information

## Future Improvements

1. **Mock PyPI Server**: Run local PyPI-compatible server
2. **Cached Responses**: Cache PyPI responses for offline testing
3. **Download Test**: Test actual file download (currently skipped for speed)
4. **Private Index**: Test custom index_url parameter
5. **Version Selection**: Test specific version queries

## References

- Test helpers: `tests/helpers/mod.rs`
- Implementation: `src/client/pypi/CLAUDE.md`
- PyPI JSON API: https://warehouse.pypa.io/api-reference/json.html

## `command_channel_test.rs` — the dashboard's `[ send ]`

One test, **zero LLM calls**. The client's LLM points at `http://127.0.0.1:1`, so the
connected-event call (where there is one) and every response-event call fail immediately;
tolerating that failure is part of what the test verifies.

The index is a throwaway `TcpListener` on 127.0.0.1 that speaks just enough HTTP/1.1 and records the request lines it saw. **Nothing contacts pypi.org.**

What it asserts, in order:

1. `wait_for_client_handle` — the regression guard for "register the channel before the
   connected-event LLM call". Without it, a manual routing rule would leave `[ send ]` dead
   for the length of the park.
2. The injected action returns `Executed`, **not** `Sent` — `reqwest` reports no wire byte count, so a number would be invented.
3. The stub index really saw `GET /pypi/requests/json`. Because `apply_action` awaits the request, the outcome cannot arrive before the bytes did.
4. The client's access log gained an `injected_action` entry, so injected traffic shows up in
   the request pane exactly like LLM-produced traffic.
5. An unknown verb comes back `Rejected`, not silently swallowed. And `search_packages` is pinned as the honest-`Executed` case: PyPI retired its search API, so the test asserts the stub saw **no new request at all** while the outcome still says what happened.
6. `disconnect` returns `Disconnected`, the command loop exits, the handle is dropped and the
   client's status becomes `Disconnected`.
