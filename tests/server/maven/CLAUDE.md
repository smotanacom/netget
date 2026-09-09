# Maven Repository E2E Testing

## Overview

End-to-end tests for the Maven repository protocol implementation. Tests validate Maven artifact serving using HTTP
requests and optionally the real Maven CLI.

**Test Runtime**: ~15-30 seconds for standard tests (without Maven CLI)
**LLM Calls**: 8 calls (4 tests × 1 server startup + 1 warmup each)

## Test Strategy

### Black-Box Testing

All tests treat NetGet as a black box:

- Spawn actual NetGet binary via process
- Pass Maven repository prompts via command line
- Validate responses using HTTP client (reqwest)
- Optionally test with real Maven CLI (`mvn`)

### Test Organization

**Test Suite 1: Simple Artifact** (`test_maven_simple_artifact`)

- Single artifact with multiple file types (JAR, POM, SHA-1, metadata)
- Tests basic Maven path parsing and response generation
- Validates 404 for missing artifacts
- **LLM Calls**: 1 (server startup)

**Test Suite 2: Multi-Version** (`test_maven_multi_version`)

- Multiple versions of the same artifact
- Tests version listing in maven-metadata.xml
- Validates version-specific artifact retrieval
- **LLM Calls**: 1 (server startup)

**Test Suite 3: Classifiers** (`test_maven_with_classifier`)

- Artifacts with classifiers (sources, javadoc)
- Tests Maven classifier path parsing
- Validates different classifier responses
- **LLM Calls**: 1 (server startup)

**Test Suite 4: Real Maven CLI** (`test_maven_cli_download`)

- Drives the actual `mvn` binary; **runs by default**, and **fails rather than skips**
  when `mvn` is missing
- Serves POM, JAR and their `.sha1` companions from one branching mock rule
- Runs `mvn dependency:get -Dartifact=com.netget.test:maven-test:1.0.0`
- Asserts the artifact Maven **stored**: the JAR and POM read back out of Maven's local
  repository must be byte-for-byte what NetGet served. Maven only writes them after
  verifying the checksums, which are computed by `shasum` rather than by NetGet
- Asserts every `Downloading from` line names 127.0.0.1
- **LLM Calls**: 1 (server startup); the artifact requests are all mock-handled
- **Requirements**: `mvn` on PATH, `shasum`, and a `~/.m2/repository` that has cached
  `maven-dependency-plugin` at least once. The test fails with an explicit message
  naming any of these

**How it stays offline.** `dependency:get` needs `maven-dependency-plugin`, which a
fresh `-Dmaven.repo.local` cannot resolve without Maven Central. Maven 3.9's *split
local repository* is the way out: writes go to a throwaway head
(`-Dmaven.repo.local`), reads fall back to the machine's existing cache
(`-Dmaven.repo.local.tail`), so plugins resolve locally and the user's real `~/.m2` is
never written to. A test-owned `settings.xml` mirrors `*` at the NetGet port, which
suppresses `central` and anything in the user's own settings.

**What this replaced.** The previous version was `#[ignore]`d, printed "Maven CLI not
found, skipping test" and returned `Ok(())`, asserted **nothing** on the happy path
(`if success { println!("✓") } else { println!("⚠ inconclusive") }`), and was started
with no `.with_mock()` — which builds a strict empty mock where every LLM call 500s, so
`start_netget_server` could never get its `open_server` action back. It could not have
passed under any circumstances, and this file described it as merely optional.

## Test Efficiency

### LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Actual**: 8 calls (4 test functions)

**Breakdown**:

- test_maven_simple_artifact: 1 startup call + 5 HTTP requests (no LLM)
- test_maven_multi_version: 1 startup call + 4 HTTP requests (no LLM)
- test_maven_with_classifier: 1 startup call + 4 HTTP requests (no LLM)
- test_maven_cli_download: 1 startup call + Maven CLI requests (no LLM)

**Optimization Strategy**:

- Each test creates ONE server instance
- Multiple artifact requests reuse same server
- HTTP requests don't trigger additional LLM calls (server already primed)
- The Maven CLI test is **not** optional: it fails when mvn is absent, because a skip-when-missing gate is a silent pass rather than evidence

### Runtime Performance

- Standard tests: ~15-30 seconds total
    - Server startup: ~2-5s per test (LLM prompt processing)
    - HTTP requests: <100ms each (no LLM, just server response)
- Maven CLI test: +10-20 seconds (if mvn is available)
    - Depends on Maven download/cache behavior

## Test Validation

### What We Test

**Maven Path Parsing**:

- GroupId with dots converted to slashes (com.example → com/example)
- ArtifactId in path
- Version in path
- Classifiers (sources, javadoc)
- File extensions (jar, pom, xml)
- Checksum files (.sha1, .md5)

**Maven Responses**:

- JAR file content (binary or text)
- POM file content (XML)
- maven-metadata.xml format and content
- SHA-1 checksums
- 404 for missing artifacts

**HTTP Protocol**:

- Status codes (200, 404)
- Content-Type headers
- Response bodies

### What We Don't Test

**Not Tested** (out of scope or future enhancements):

- Maven deploy (PUT requests) - read-only repository
- SNAPSHOT versioning with timestamps
- Binary JAR file serving (text used for simplicity)
- Automatic checksum generation (LLM provides checksums)
- Repository mirroring or proxying
- Authentication/authorization
- HTTPS/TLS connections

## Running the Tests

### Prerequisites

- Rust toolchain installed
- NetGet compiled in release mode: `./cargo-isolated.sh build --release --no-default-features --features maven`
- **Required**: the Maven CLI on PATH, `shasum`, and a `~/.m2/repository` that has cached `maven-dependency-plugin` (warm it once with `mvn -B dependency:get -Dartifact=junit:junit:4.13.2`)

### Run Maven Tests Only

```bash
# Every Maven test, the real-CLI one included — nothing here is ignored
./cargo-isolated.sh test --no-default-features --features maven --test server \
    -- server::maven --test-threads=100
```

### Important Notes

- **Always use `--no-default-features --features maven`** - never use `--all-features` (slow!)
- **Never run all tests together** - use protocol-specific features
- Build isolation with `cargo-isolated.sh` prevents conflicts with other instances
- Tests run in parallel by default (safe due to random ports and a per-test mock LLM)

## Privacy and Offline Testing

**All tests are localhost-only**:

- No external network requests
- No real Maven Central access
- Works completely offline
- Binds to 127.0.0.1 only

**Maven CLI test**:

- Resolves everything through 127.0.0.1: a test-owned `settings.xml` mirrors `*` at the
  NetGet port, and the test asserts no `Downloading from` line names anything else
- Writes into a throwaway local repository, never into `~/.m2/repository`. It *reads*
  from `~/.m2` as the split-repository tail so plugins resolve without Maven Central
- Does NOT contact Maven Central, for the test artifact or for anything else

## Known Issues and Limitations

### Issue 1: Maven CLI Caching

**Problem**: Maven CLI may cache 404 responses, preventing retry
**Workaround**: Use -U flag (force update) or unique artifact coordinates per test
**Status**: Acceptable - tests use unique coordinates

### Issue 2: Binary JAR Files

**Problem**: Tests use text content instead of actual JAR files
**Rationale**: Simplifies LLM generation and test validation
**Impact**: Still validates path parsing and HTTP serving, just not binary content
**Status**: Acceptable for MVP testing

### Issue 3: Checksum Validation

**Problem**: Tests use fake checksums (abc123), not real SHA-1 hashes
**Rationale**: LLM doesn't automatically calculate checksums
**Impact**: Validates checksum file serving, not checksum accuracy
**Status**: Acceptable - checksum generation is future enhancement

### Issue 4: Test Flakiness

**Problem**: LLM responses may vary slightly (wording, formatting)
**Mitigation**: Assertions are flexible (contains checks, not exact matches)
**Status**: Tests are robust to reasonable LLM variation

## Future Test Enhancements

### Additional Test Coverage

1. **Larger artifact repositories**: Test with 10+ artifacts
2. **Real binary JARs**: Use actual compiled JAR files
3. **Checksum validation**: Verify SHA-1/MD5 match content
4. **Multiple classifiers**: Test with more classifier types
5. **Error handling**: Test malformed requests, large files, timeouts

### Performance Testing

1. **Concurrent requests**: Test 10+ parallel Maven requests
2. **Large files**: Test with 100MB+ JAR files
3. **Latency**: Measure LLM response time distribution

### Integration Testing

1. **Gradle compatibility**: Test with Gradle instead of Maven
2. **IDE integration**: Test with IntelliJ IDEA or Eclipse Maven plugin
3. **CI/CD pipelines**: Test in GitHub Actions or Jenkins

## References

- [Maven Repository Layout](https://maven.apache.org/repository/layout.html)
- [Maven CLI Documentation](https://maven.apache.org/ref/current/maven-embedder/cli.html)
- [NetGet Test Infrastructure](../../README.md)
- [Implementation CLAUDE.md](../../../src/server/maven/CLAUDE.md)
