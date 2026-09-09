# S3 Client E2E Test Strategy

## Test Approach

The S3 client tests verify LLM-controlled interactions with AWS S3 and S3-compatible services (MinIO, LocalStack).

## Test Environment Setup

### Option 1: MinIO (Recommended for Local Testing)

**Run MinIO in Docker:**

```bash
docker run -d \
  -p 9000:9000 \
  -p 9001:9001 \
  --name minio \
  -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin \
  quay.io/minio/minio server /data --console-address ":9001"
```

**Create test bucket:**

```bash
# Install MinIO client
brew install minio/stable/mc  # macOS
# or download from https://min.io/docs/minio/linux/reference/minio-mc.html

# Configure MinIO client
mc alias set local http://localhost:9000 minioadmin minioadmin

# Create test bucket
mc mb local/test-bucket
```

**Test credentials:**

- Endpoint: `http://localhost:9000`
- Access Key: `minioadmin`
- Secret Key: `minioadmin`
- Region: `us-east-1` (can be any value for MinIO)

### Option 2: LocalStack (AWS Emulator)

**Run LocalStack in Docker:**

```bash
docker run -d \
  -p 4566:4566 \
  --name localstack \
  -e SERVICES=s3 \
  localstack/localstack
```

**Test credentials:**

- Endpoint: `http://localhost:4566`
- Access Key: `test`
- Secret Key: `test`
- Region: `us-east-1`

### Option 3: Real AWS S3 (CI/Production Tests)

**Prerequisites:**

- AWS account with S3 access
- IAM user with S3 permissions (ListBuckets, GetObject, PutObject, DeleteObject)
- AWS credentials configured (`~/.aws/credentials` or environment variables)

**Create test bucket:**

```bash
aws s3 mb s3://netget-test-bucket-$(uuidgen | tr '[:upper:]' '[:lower:]')
```

**Clean up after tests:**

```bash
aws s3 rb s3://netget-test-bucket-xxx --force
```

## Test Strategy

### Black-Box Testing

All tests use the compiled `netget` binary as a black box:

- Spawn NetGet with S3 client instructions
- Monitor output for expected behavior
- Verify LLM-controlled actions execute correctly

### Test Categories

**1. Connection Tests**

- Verify S3 client initialization
- Test endpoint configuration (AWS, MinIO, LocalStack)
- Validate credential handling

**2. Bucket Operations**

- List buckets
- Create bucket
- Delete bucket

**3. Object Operations**

- Put object (upload)
- Get object (download)
- Head object (metadata)
- Delete object
- List objects

**4. Error Handling**

- Invalid credentials
- Non-existent bucket
- Non-existent object
- Permission denied

## LLM Call Budget

**Target:** < 10 LLM calls per test suite

### Current Budget Breakdown

| Test                                 | LLM Calls | Rationale             |
|--------------------------------------|-----------|-----------------------|
| `test_s3_client_connect`             | 1         | Client initialization |
| `test_s3_client_list_buckets`        | 2         | Init + list buckets   |
| `test_s3_client_object_operations`   | 3         | Init + put + get      |
| `test_s3_client_invalid_credentials` | 1         | Init with bad creds   |
| **Total**                            | **7**     | ✅ Under budget        |

### Optimization Strategies

1. **Reuse connections** - Multiple operations in single test
2. **Batch operations** - Put multiple objects in one instruction
3. **Mock responses** - For error scenarios, don't need real S3

## Expected Runtime

**With MinIO (local):**

- Per test: 2-5 seconds
- Full suite: 15-30 seconds

**With LocalStack:**

- Per test: 3-6 seconds
- Full suite: 20-35 seconds

**With real AWS S3:**

- Per test: 5-10 seconds (network latency)
- Full suite: 30-60 seconds

## Test Execution

### Run All S3 Client Tests

```bash
# With MinIO running on localhost:9000
./cargo-isolated.sh test --no-default-features --features s3 --test client -- client::s3
```

### Run Specific Test

```bash
./cargo-isolated.sh test --no-default-features --features s3 --test client -- client::s3::e2e_test::s3_client_tests::test_s3_client_connect --exact --ignored
```

### Run with AWS S3

```bash
# Set AWS credentials
export AWS_ACCESS_KEY_ID=your_access_key
export AWS_SECRET_ACCESS_KEY=your_secret_key
export AWS_REGION=us-east-1

./cargo-isolated.sh test --no-default-features --features s3 --test client -- client::s3
```

## What actually runs

**One test runs by default: `command_channel_test.rs`.** All four tests in `e2e_test.rs` are
`#[ignore]`d (three need MinIO or LocalStack, one needs `--use-ollama`), so every budget and
coverage figure below describes tests that do not execute. The LLM-call budget for a default
run is **zero**, not the seven counted below.

`command_channel_test.rs` — which this file never mentioned — proves the dashboard's
`[ send ]` path end to end against a loopback stub: an injected `list_buckets` becomes a real
HTTP request, an unknown verb is `Rejected`, `disconnect` drops the handle, and the
credentials and region from the startup parameters appear in the `Authorization` header the
stub receives.

Two corrections to the commands below: the feature is **`s3`**, not `s3-client`, which does not
exist in `Cargo.toml` — every command in this file used to fail on that alone. And `--test`
names a cargo *target* (`client`, i.e. `tests/client.rs`); the module path goes after `--`.

## Known Issues & Limitations

### 1. Binary Data Not Supported

**Issue:** LLMs cannot work with binary data directly.

**Workaround:** Tests only use text content. For binary, would need base64 encoding.

**Test impact:** Binary upload/download tests are not included.

### 2. Large Object Tests Skipped

**Issue:** Memory constraints for objects >100MB.

**Workaround:** Tests use small objects (<1KB).

**Test impact:** No multipart upload tests, no streaming tests.

### 3. Requires External Service

**Issue:** Tests are marked `#[ignore]` because they require MinIO/LocalStack.

**Solution:** CI pipeline should run MinIO in Docker before tests.

**Test impact:** Tests don't run by default (`cargo test` skips them).

### 4. Pagination Not Tested

**Issue:** `list_objects` pagination not fully tested.

**Workaround:** Tests only verify first page of results.

**Test impact:** Large bucket listing not validated.

### 5. Advanced Features Not Tested

Not tested due to complexity or LLM limitations:

- Multipart uploads
- Presigned URLs
- Object versioning
- Server-side encryption
- Object ACLs
- S3 Select queries

## CI Integration

### GitHub Actions Example

```yaml
jobs:
  test-s3-client:
    runs-on: ubuntu-latest
    services:
      minio:
        image: quay.io/minio/minio
        ports:
          - 9000:9000
        env:
          MINIO_ROOT_USER: minioadmin
          MINIO_ROOT_PASSWORD: minioadmin
        options: >-
          --health-cmd "curl -f http://localhost:9000/minio/health/live"
          --health-interval 10s
          --health-timeout 5s
          --health-retries 3

    steps:
      - uses: actions/checkout@v3
      - name: Install Rust
        uses: actions-rs/toolchain@v1
      - name: Create test bucket
        run: |
          wget https://dl.min.io/client/mc/release/linux-amd64/mc
          chmod +x mc
          ./mc alias set local http://localhost:9000 minioadmin minioadmin
          ./mc mb local/test-bucket
      - name: Run S3 client tests
        run: ./cargo-isolated.sh test --no-default-features --features s3 --test client -- client::s3
```

## Test Maintenance

### When to Update Tests

- **New S3 operations added** - Add corresponding test
- **Error handling changes** - Update error scenario tests
- **AWS SDK version update** - Verify compatibility
- **MinIO version update** - Test against new version

### Test Health Metrics

- **Pass rate:** Should be >95% (flaky network may cause failures)
- **Runtime:** Should stay <60 seconds for full suite
- **LLM calls:** Should stay <10 per suite

## Debugging Failed Tests

### 1. Connection Failures

**Symptom:** "Failed to connect to S3"

**Checks:**

- MinIO/LocalStack running? (`docker ps`)
- Correct endpoint? (`http://localhost:9000` not `https://`)
- Credentials correct? (check Docker logs)

### 2. Bucket Not Found

**Symptom:** "NoSuchBucket"

**Checks:**

- Test bucket created? (`mc ls local/`)
- Bucket name correct? (no typos)
- Region matches? (MinIO accepts any region)

### 3. Permission Denied

**Symptom:** "AccessDenied"

**Checks:**

- Credentials valid? (test with `mc` client)
- IAM policy correct? (for AWS S3)
- Bucket policy allows access? (for AWS S3)

### 4. Timeout

**Symptom:** Test hangs or times out

**Checks:**

- LLM responding? (check Ollama logs)
- Network issues? (check Docker network)
- S3 service slow? (check MinIO/LocalStack logs)

## Future Improvements

1. **Docker Compose setup** - Automatic MinIO startup for tests
2. **Binary data tests** - Add base64 encode/decode support
3. **Streaming tests** - Test large file uploads/downloads
4. **Pagination tests** - Test `list_objects` continuation tokens
5. **Presigned URL tests** - Generate and use presigned URLs
6. **Multipart upload tests** - Test large file uploads
7. **Error recovery tests** - Test retry logic and error handling
8. **Performance tests** - Benchmark S3 operations
9. **Concurrent operation tests** - Multiple parallel uploads/downloads
10. **S3 Select tests** - Query objects with SQL

## Related Files

- `src/client/s3/mod.rs` - S3 client implementation
- `src/client/s3/actions.rs` - Action definitions
- `src/client/s3/CLAUDE.md` - Implementation documentation
- `tests/client/s3/e2e_test.rs` - E2E test suite (this file's tests)
