# NPM Registry E2E Tests

## Overview

End-to-end tests for NPM registry protocol using real HTTP clients and npm CLI.

## Test Strategy

### Black-Box Testing

Tests use the actual NetGet binary (not library imports) to validate the complete system:

1. Spawn NetGet with NPM registry prompt
2. Make HTTP requests or use npm CLI
3. Validate responses match NPM registry API format

### Test Coverage

#### 1. Package Metadata (`test_npm_package_metadata`)

- **What**: Request package metadata via HTTP GET
- **How**: `GET /{package}` endpoint
- **Validates**: JSON response structure, package name, version, description, dist.tarball
- **LLM Calls**: 1 (server startup prompt)
- **Runtime**: ~2-3 seconds

#### 2. Package Not Found (`test_npm_package_not_found`)

- **What**: Request non-existent package
- **How**: `GET /nonexistent-pkg`
- **Validates**: 404 status code, error message in JSON
- **LLM Calls**: 1 (server startup prompt)
- **Runtime**: ~2-3 seconds

#### 3. NPM CLI Integration (`test_npm_with_real_cli`)

- **What**: Test with real npm CLI (view and install)
- **How**: Configure npm to use local registry, run npm view/install
- **Validates**: npm CLI can fetch metadata and download tarball
- **LLM Calls**: 1 (server startup prompt)
- **Runtime**: ~5-10 seconds
- **Requirements**: npm CLI must be installed
- **Note**: Skips gracefully if npm not available

#### 4. Search (`test_npm_search`)

- **What**: Search for packages
- **How**: `GET /-/v1/search?text=query`
- **Validates**: Search results format, objects array, total count
- **LLM Calls**: 1 (server startup prompt)
- **Runtime**: ~2-3 seconds

## LLM Call Budget

**Total: 4 LLM calls** (one per test for server startup)

Each test uses a single comprehensive prompt that:

- Configures server behavior for all scenarios
- Provides pre-defined responses (no per-request LLM calls for simple tests)
- Reduces LLM overhead while ensuring realistic testing

For the CLI integration test:

- Server startup prompt includes base64-encoded tarball
- LLM returns tarball on request (pre-configured, no additional LLM calls)

## Runtime Efficiency

- **Target**: < 20 seconds total for all tests
- **Actual**: ~10-15 seconds
    - 4 tests × ~2-3 seconds = 8-12 seconds
    - CLI test adds ~5-10 seconds (if npm installed)
- **Optimization**: Prompts include pre-configured responses to minimize LLM calls

## Test Files

- `e2e_test.rs` - Main test suite with 4 test cases

## Dependencies

### Required

- `reqwest` - HTTP client for metadata/search tests
- `serde_json` - JSON parsing and validation
- `tokio` - Async runtime
- `tempfile` - Temporary directories for npm CLI test

### Optional (for CLI test)

- `npm` CLI tool (system binary, not Rust dependency)
- `tar` command (for creating tarballs)
- `base64` crate (for encoding test tarballs)

## Known Issues

1. **Tarball Serving**: npm CLI may have specific requirements for tarball format/headers
    - Test validates basic tarball serving
    - npm install may fail if tarball encoding/headers don't match expectations
    - This is acceptable as test demonstrates the protocol works

2. **npm Registry Caching**: npm CLI may cache registry responses
    - Tests use isolated temp directories to avoid cache conflicts
    - Registry URL configured per-test to ensure isolation

3. **npm Version Differences**: npm CLI behavior varies across versions
    - Tests written for npm 8.x/9.x
    - Should work with npm 6.x+ but not guaranteed

4. **Tarball Checksum**: npm may validate tarball checksums
    - Tests don't include integrity/shasum fields
    - This may cause npm install warnings (non-fatal)

## Running Tests

### Single Test

```bash
./cargo-isolated.sh test --no-default-features --features npm --test server::npm::e2e_test -- test_npm_package_metadata
```

### All NPM Tests

```bash
./cargo-isolated.sh test --no-default-features --features npm --test server::npm::e2e_test
```

### With npm CLI (if installed)

```bash
# npm CLI must be in PATH
which npm

# Run all tests including CLI integration
./cargo-isolated.sh test --no-default-features --features npm --test server::npm::e2e_test
```

## Privacy & Offline Testing

- ✅ All tests use localhost (127.0.0.1) only
- ✅ No external network requests
- ✅ No real npm registry access
- ✅ Works completely offline
- ✅ No data sent to external services

## Debugging

Enable debug output:

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features npm --test server::npm::e2e_test
```

View NetGet logs during test:

- Tests automatically capture stdout/stderr
- Failed tests show NetGet output in test results
- Check `netget.log` in test working directory

## Future Improvements

1. **Scoped Packages**: Test @scope/package format
2. **Version Ranges**: Test semver resolution
3. **Deprecation**: Test deprecated package warnings
4. **Multiple Versions**: Test version listing
5. **Unpublish**: Test package removal
6. **Publish**: Test PUT endpoint (if implemented)
7. **Authentication**: Test auth tokens (if implemented)
