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

- Tests with actual `mvn` command (if available)
- Creates temporary project with pom.xml
- Configures custom repository pointing to NetGet
- Runs `mvn dependency:resolve`
- **LLM Calls**: 1 (server startup)
- **Status**: Marked with `#[ignore]` - requires Maven installation

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
- Maven CLI test is optional (requires mvn installation)

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
- Optional: Maven CLI installed (for test_maven_cli_download)

### Run Maven Tests Only

```bash
# Run all Maven tests (except Maven CLI test)
./cargo-isolated.sh test --no-default-features --features maven --test maven

# Run with Maven CLI test (requires mvn)
./cargo-isolated.sh test --no-default-features --features maven --test maven -- --ignored
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

- Uses local repository (127.0.0.1)
- May cache artifacts in ~/.m2/repository
- Does NOT contact Maven Central for test artifact

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
