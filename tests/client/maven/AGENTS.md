# Maven Client E2E Test Strategy

## Test Approach

Maven client tests verify LLM-controlled artifact discovery and download from Maven repositories. Tests use **real Maven
Central** repository to ensure realistic behavior.

## Test Philosophy

**Black-box testing**: Tests spawn the actual NetGet binary and verify client behavior through output inspection. No
mocking of Maven repositories - all tests hit real Maven Central endpoints.

## LLM Call Budget

**Target**: < 6 LLM calls total across all tests
**Actual**: 5 LLM calls

### Test Breakdown

1. **test_maven_client_download_artifact** - 1 LLM call
    - Client connects to Maven Central
    - Downloads org.apache.commons:commons-lang3:3.12.0

2. **test_maven_client_download_pom** - 1 LLM call
    - Client connects to Maven Central
    - Downloads junit:junit:4.13.2 POM file

3. **test_maven_client_search_versions** - 1 LLM call
    - Client connects to Maven Central
    - Fetches maven-metadata.xml for com.google.guava:guava

4. **test_maven_client_custom_repository** - 1 LLM call
    - Client connects with explicit repository URL
    - Downloads commons-io:commons-io:2.11.0

5. **test_maven_client_missing_artifact** - 1 LLM call
    - Client attempts to download non-existent artifact
    - Verifies error handling (404 response)

## Expected Runtime

**Total**: ~15-20 seconds for all tests (with Ollama local)

- Per test: 3-4 seconds (connection + download + LLM processing)
- Network latency to Maven Central: ~500ms per request
- LLM processing: ~1-2s per call

## Test Artifacts Used

All artifacts chosen for reliability and stability:

- **commons-lang3:3.12.0** - Popular, stable, well-known artifact
- **junit:junit:4.13.2** - Widely used testing framework
- **com.google.guava:guava** - Popular utility library with many versions
- **commons-io:commons-io:2.11.0** - Apache Commons library

These artifacts are guaranteed to exist in Maven Central and won't be deleted.

## Test Coverage

### Functional Coverage

- ✅ Artifact download (JAR files)
- ✅ POM file download
- ✅ Version metadata search
- ✅ Custom repository URL
- ✅ Error handling (404 responses)

### NOT Covered

- ❌ Dependency resolution (complex, requires POM parsing by LLM)
- ❌ Transitive dependency download (multi-step LLM flow)
- ❌ Authenticated repositories (no credentials support yet)
- ❌ Local repository caching (not implemented)
- ❌ Checksum verification (not implemented)
- ❌ SNAPSHOT versions (time-dependent, flaky tests)

## Known Issues

### Flaky Tests

**None expected** - Tests use stable public artifacts from Maven Central.

Potential issues:

- **Network failures**: Maven Central is highly available (99.9%+)
- **Slow downloads**: Large artifacts may timeout (mitigated by choosing small artifacts)
- **LLM interpretation**: If Ollama model changes, LLM may not generate expected actions

### Mitigation Strategies

1. **Small artifacts**: All test artifacts < 1MB to avoid timeout
2. **Stable versions**: No SNAPSHOT versions to avoid time-dependent failures
3. **Generous timeouts**: 3s per test allows for network variance
4. **Error tolerance**: Tests check for Maven protocol or error messages

## Test Isolation

Tests are fully isolated:

- No shared state between tests
- Each test spawns independent NetGet client process
- No local file system dependencies
- No mock servers required (uses real Maven Central)

## Running Tests

```bash
# Run all Maven client tests
./cargo-isolated.sh test --no-default-features --features maven --test client -- client::maven::e2e_test

# Run specific test
./cargo-isolated.sh test --no-default-features --features maven --test client -- client::maven::e2e_test::test_maven_client_download_artifact
```

## Debugging Failed Tests

If tests fail:

1. **Check network connectivity**: `curl https://repo.maven.apache.org/maven2/`
2. **Verify Ollama running**: `curl http://localhost:11434/api/tags`
3. **Check test output**: Look for ERROR messages in client output
4. **Increase timeout**: Slow networks may need longer waits
5. **Verify artifact exists**: Check Maven Central web UI

## Future Enhancements

### Additional Test Scenarios

1. **Dependency resolution**: Test LLM parsing POM dependencies
2. **Multi-artifact download**: Test downloading artifact + all dependencies
3. **Version range resolution**: Test "find latest version" scenarios
4. **Repository fallback**: Test trying multiple repositories
5. **Parallel downloads**: Test concurrent artifact downloads

### Test Infrastructure

1. **Local Maven repository**: Use local Nexus/Artifactory for faster tests
2. **Mock Maven server**: Test edge cases without network dependency
3. **Performance benchmarks**: Measure download speed and LLM latency
4. **Snapshot testing**: Validate POM/metadata XML parsing

## Security Testing

Not included in E2E tests (handled separately):

- HTTPS certificate validation
- Malicious XML in POM files (LLM treats as text)
- Path traversal in artifact coordinates
- Denial of service via large downloads

## Performance Expectations

| Metric             | Target  | Measured |
|--------------------|---------|----------|
| LLM calls          | < 6     | 5        |
| Total runtime      | < 30s   | ~15-20s  |
| Network requests   | ~5-10   | 5        |
| Memory usage       | < 100MB | TBD      |
| Download bandwidth | < 5MB   | < 1MB    |

## Artifact Selection Criteria

Chosen artifacts meet these criteria:

1. **Stable**: Well-established, won't be deleted
2. **Small**: < 1MB download size
3. **Popular**: High download counts (reliability indicator)
4. **Diverse**: Different groups, different use cases
5. **Versioned**: Multiple versions available for search tests

## Test Output Validation

Tests verify:

1. **Protocol detection**: Output contains "Maven" or "maven"
2. **Operation confirmation**: Output shows artifact/POM/version messages
3. **Error handling**: Graceful 404 handling for missing artifacts
4. **Connection success**: No fatal errors or crashes

## Comparison with Other Client Tests

Similar to HTTP client tests but:

- **No local server needed**: Uses public Maven Central
- **More structured data**: Maven coordinates vs free-form URLs
- **XML parsing by LLM**: POM/metadata parsed as text
- **Higher latency**: Remote repository vs localhost

## Continuous Integration

Tests suitable for CI/CD:

- ✅ No local server setup required
- ✅ Reproducible (stable artifacts)
- ✅ Fast (< 30s total)
- ✅ No authentication needed
- ❌ Requires internet access to Maven Central
- ❌ Requires Ollama running

For offline CI, consider:

1. Use local Maven repository mirror
2. Update client config to point to local URL
3. Pre-populate mirror with test artifacts

## `command_channel_test.rs` — the dashboard's `[ send ]`

One test, **zero LLM calls**. The client's LLM points at `http://127.0.0.1:1`, so the
connected-event call (where there is one) and every response-event call fail immediately;
tolerating that failure is part of what the test verifies.

The repository is a throwaway `TcpListener` on 127.0.0.1 that speaks just enough HTTP/1.1 and records the request lines it saw. **Nothing contacts repo.maven.apache.org.**

What it asserts, in order:

1. `wait_for_client_handle` — the regression guard for "register the channel before the
   connected-event LLM call". Without it, a manual routing rule would leave `[ send ]` dead
   for the length of the park.
2. The injected action returns `Executed`, **not** `Sent` — `reqwest` reports no wire byte count, so a number would be invented.
3. The stub repository really saw `GET /org/example/demo/1.0/demo-1.0.pom`, and `search_versions` really fetched `maven-metadata.xml` — both through the same `apply_action`.
4. The client's access log gained an `injected_action` entry, so injected traffic shows up in
   the request pane exactly like LLM-produced traffic.
5. An unknown verb comes back `Rejected`, not silently swallowed.
6. `disconnect` returns `Disconnected`, the command loop exits, the handle is dropped and the
   client's status becomes `Disconnected`.

## In the browser

The native suites here drive the reqwest backend. The browser path — the same client code over
`src/client/http_fetch`'s hyper transport — is proven by `web/test/smoke.mjs` against the real
bundle (see `src/client/maven/AGENTS.md`, "Browser build"), and the transport backend's requests
are pinned to reqwest's by `tests/client/http/fetch_client_test.rs`.

`command_channel_test.rs::a_bare_repository_address_is_given_a_scheme` pins the connect-time
normalisation: `127.0.0.1:1` is stored as `https://127.0.0.1:1`, and an address with a scheme
is kept as given. Making the bare branch keep the address unchanged turns it red.

