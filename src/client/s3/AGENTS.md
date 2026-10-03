# S3 Client Implementation

## Overview

The S3 client provides LLM-controlled access to AWS S3 and S3-compatible object storage services (MinIO, LocalStack,
etc.). It uses the official AWS SDK for Rust (`aws-sdk-s3`) to perform bucket and object operations.

## Library Choice: AWS SDK for Rust

**Crate:** `aws-sdk-s3` v1.x
**Why:** Official AWS SDK with comprehensive S3 API coverage, automatic authentication, and retry logic.

**Alternatives Considered:**

- `rusoto_s3` - Deprecated in favor of AWS SDK
- Raw HTTP with AWS Signature v4 - Too complex, reinventing the wheel
- `s3-rust` - Less mature, limited features

**Dependency:**

```toml
aws-config = "1.5"
aws-sdk-s3 = "1.90"   # Cargo.toml, not 1.55; feature name is `s3`, not `s3-client`
```

## Architecture

### Connection Model

Unlike TCP-based protocols, S3 is HTTP-based and connectionless. The "connection" in this context means:

1. **Configuration initialization** - AWS credentials, region, endpoint URL
2. **Logical client creation** - Building the AWS SDK S3 client
3. **On-demand operations** - Each action makes a separate HTTP(S) request

**No persistent connection** is maintained. Each S3 operation (put, get, list, etc.) creates a new HTTP request.

### State Management

The client stores configuration in protocol_data:

- `endpoint`: S3 endpoint URL (AWS or custom like MinIO)
- `region`: AWS region (e.g., us-east-1)
- `access_key_id`: AWS access key, read from `ctx.startup_params` at connect
- `secret_access_key`: AWS secret key, read from `ctx.startup_params` at connect

Both are then mirrored into `protocol_data`, which is where each operation re-reads them.
That mirror used to be the *only* source: `connect()` dropped `ctx.startup_params` on the
floor and read `protocol_data`, which nothing populates before connect — `client_startup.rs`
builds the instance with `protocol_data: Null` and the only writer is the block that runs
after the read. So every S3 client signed with `Credentials::new("", "", ...)`, was pinned to
`us-east-1`, and could never reach a custom endpoint, while the protocol advertised all four
parameters and marked two of them required.
`tests/client/s3/command_channel_test.rs::startup_credentials_reach_the_signature` pins it by
looking for the access key id and region in the `Authorization` header the stub receives.

**No connection state machine** (Idle/Processing/Accumulating) is needed because operations are request-response style
with no streaming or persistent connections.

### Authentication

Uses AWS Signature Version 4 authentication, handled automatically by the SDK:

- **Access Key ID** - Public identifier
- **Secret Access Key** - Secret for signing requests
- **Session Token** - Optional for temporary credentials (not implemented yet)

Credentials are passed via `Credentials::new()` to the SDK config builder.

### Custom Endpoints

Supports S3-compatible services via `endpoint_url` parameter:

```rust
let config = aws_sdk_s3::config::Builder::new()
    .region(Region::new("us-east-1"))
    .credentials_provider(creds)
    .endpoint_url("http://localhost:9000")  // MinIO
    .build();
```

**Use cases:**

- MinIO (local object storage)
- LocalStack (AWS emulator)
- Ceph (open-source storage)
- Wasabi (cloud storage)

## LLM Integration

### Events

**1. `s3_connected`** - Triggered after client initialization

```json
{
  "endpoint": "s3.amazonaws.com",
  "region": "us-east-1"
}
```

**2. `s3_response_received`** - Triggered after each S3 operation

```json
{
  "operation": "s3_put_object",
  "success": true,
  "result": {
    "bucket": "my-bucket",
    "key": "data/file.txt",
    "etag": "\"d41d8cd98f00b204e9800998ecf8427e\""
  }
}
```

Or on error:

```json
{
  "operation": "s3_get_object",
  "success": false,
  "error": "NoSuchKey: The specified key does not exist"
}
```

### Actions

#### Async Actions (User-triggered)

1. **put_object** - Upload object
   ```json
   {
     "type": "put_object",
     "bucket": "my-bucket",
     "key": "data/file.txt",
     "body": "Hello, S3!",
     "content_type": "text/plain"
   }
   ```

2. **get_object** - Download object
   ```json
   {
     "type": "get_object",
     "bucket": "my-bucket",
     "key": "data/file.txt"
   }
   ```

3. **list_buckets** - List all buckets
   ```json
   {
     "type": "list_buckets"
   }
   ```

4. **list_objects** - List objects in bucket
   ```json
   {
     "type": "list_objects",
     "bucket": "my-bucket",
     "prefix": "data/",
     "max_keys": 100
   }
   ```

5. **delete_object** - Delete object
   ```json
   {
     "type": "delete_object",
     "bucket": "my-bucket",
     "key": "data/file.txt"
   }
   ```

6. **head_object** - Get object metadata
   ```json
   {
     "type": "head_object",
     "bucket": "my-bucket",
     "key": "data/file.txt"
   }
   ```

7. **create_bucket** - Create bucket
   ```json
   {
     "type": "create_bucket",
     "bucket": "my-new-bucket"
   }
   ```

8. **delete_bucket** - Delete bucket (must be empty)
   ```json
   {
     "type": "delete_bucket",
     "bucket": "my-old-bucket"
   }
   ```

#### Sync Actions (Response-triggered)

Same as async actions, allowing the LLM to chain operations:

- After uploading, list objects to verify
- After getting an object, process and upload result
- After listing, download specific objects

### Action Execution Flow

1. **LLM generates action** (via `call_llm_for_client`)
2. **Action parsed** in `execute_action()` → returns `ClientActionResult::Custom`
3. **Operation executed** in `execute_operation()` via AWS SDK
4. **LLM called with result** via `s3_response_received` event
5. **LLM decides next action** (or waits for user input) — but only once. Follow-ups from
   `s3_response_received` are dispatched through `run_operation_once`, which raises no event,
   so the chain is exactly one step deep. That is the "non-notifying path" the root
   `CLAUDE.md` names, chosen here to keep the async type non-recursive for `tokio::spawn`'s
   `Send` bound; the prescribed fix is a boxed call with a depth bound, not silence.

## Limitations

### 1. Binary Data Handling

**Issue:** LLMs cannot directly work with binary data.

**Solution:** For text files, pass content as-is. For binary files:

- **Upload:** `put_object` takes `encoding: "base64"` and decodes `body` into the object's
  bytes; `"utf8"` (the default) stores the characters as written. There is no sniffing — a
  string can be valid text and valid base64 at once, and only the sender knows which it means.
- **Download:** `get_object` returns the bytes base64-encoded when they are not valid UTF-8,
  and says which in the event.

Both are implemented. This section used to say neither was — and the `encoding` half was
*half* true in a way worth remembering: `put_object` read and decoded the field correctly,
but `execute_action` never copied `encoding` out of the model's action into the operation's
data, so the value never arrived, the `"utf8"` default always won, and a model following the
documentation stored literal base64 ASCII in the object. That is the `send_tcp_data` defect
recreated one layer up, in the executor rather than the operation, and a declared-example test
cannot catch it: the example round-trips fine because the extra field is simply ignored.

### 2. Large Files

**Issue:** AWS SDK loads entire object into memory.

**Solution (not implemented):**

- Streaming uploads via `ByteStream`
- Multipart uploads for large files (>5MB)
- Presigned URLs for direct client-side uploads/downloads

**Current limitation:** Not suitable for files >100MB due to memory constraints.

### 3. Pagination

**Issue:** `list_objects` may be truncated for large buckets.

**Solution (partially implemented):**

- Check `is_truncated` in response
- Use continuation tokens for next page (not implemented yet)

**Current limitation:** Only first page of results is returned.

### 4. Advanced Features Not Implemented

- **Multipart uploads** - For files >5MB
- **Presigned URLs** - For direct client access
- **Object versioning** - Versioned buckets
- **Object lifecycle** - Expiration policies
- **Access control** - Bucket/object ACLs
- **Server-side encryption** - SSE-S3, SSE-KMS
- **Object tagging** - Metadata tags
- **Select queries** - S3 Select for filtering

These can be added incrementally as needed.

## Error Handling

AWS SDK errors are captured and returned via `s3_response_received` event:

- **NoSuchBucket** - Bucket doesn't exist
- **NoSuchKey** - Object doesn't exist
- **AccessDenied** - Insufficient permissions
- **InvalidAccessKeyId** - Bad credentials
- **BucketAlreadyExists** - Bucket name taken
- **BucketNotEmpty** - Can't delete non-empty bucket

LLM sees the error and can decide to retry, create bucket, or report to user.

## Testing Strategy

See `tests/client/s3/CLAUDE.md` for E2E testing approach.

**Recommended test setup:**

- **Local:** MinIO or LocalStack (S3-compatible, no AWS costs)
- **CI:** LocalStack in Docker container
- **Prod:** Real AWS S3 with test bucket (use IAM role with limited permissions)

## Example Prompts

**1. Upload file:**

```
Connect to S3 at localhost:9000 (MinIO) and upload a file to bucket "test-bucket" with key "hello.txt" and content "Hello, World!"
```

**2. List and download:**

```
Connect to AWS S3 in us-east-1, list all objects in bucket "my-data", then download the first object
```

**3. Backup workflow:**

```
Connect to S3, create bucket "backup-2024", upload three files: config.json, data.csv, and readme.txt
```

**4. Cleanup:**

```
Connect to S3, list all objects in bucket "temp-bucket", delete all objects, then delete the bucket
```

## Security Considerations

1. **Credentials in memory** - Access keys are stored in protocol_data (consider encryption)
2. **Logging** - Avoid logging access keys or secret keys (currently safe)
3. **Public buckets** - Be careful not to make buckets public accidentally
4. **IAM policies** - Use least-privilege access (read-only for testing)

## Future Enhancements

1. **Streaming support** - For large files
2. **Multipart uploads** - For files >5MB
3. **Presigned URLs** - Generate URLs for direct access
4. **Binary support** - Base64 encode/decode for binary content
5. **Pagination** - Full support for large bucket listings
6. **Versioning** - Support versioned buckets
7. **Encryption** - Server-side and client-side encryption
8. **Temporary credentials** - STS AssumeRole support
9. **S3 Select** - Query objects with SQL
10. **Event notifications** - Trigger on object changes (via SQS/SNS)

## Related Files

- `src/client/s3/actions.rs` - Action definitions and trait implementation
- `src/client/s3/mod.rs` - Core S3 client logic (this file's implementation)
- `tests/client/s3/e2e_test.rs` - E2E tests with MinIO/LocalStack
- `tests/client/s3/CLAUDE.md` - Test strategy and budget

## Command channel (dashboard `[ send ]`)

`AppState::send_to_client` can inject an action into a running S3 client.
`connect_with_llm_actions` registers the channel (`command_support::register_command_channel`)
**before** the connected-event LLM call and spawns a registered `command_loop` task; the loop
also replaces the old 5s "has this client been removed yet" poll, because dropping the client
drops the handle and `recv()` returns `None` immediately.

Injected actions go through the same `S3Client::apply_action` the connected-event path uses, so
the wire encoding of every verb exists exactly once. `execute_operation` returns the operation's
own JSON result rather than `()` so the outcome can say what happened.

Outcome semantics — the AWS SDK owns the socket and reports no wire byte count, so **`Sent` is
never returned**; a fabricated byte count would be worse than an honest `Executed`:

| Situation | `ClientSendOutcome` |
|---|---|
| `execute_action` refused it (unknown verb, missing field) | `Rejected { error }` |
| Operation ran and the service answered | `Executed { detail: "s3_get_object completed: {…}" }` |
| Operation ran and the service or the SDK failed | `Executed { detail: "s3_get_object failed: …" }` |
| `disconnect` | `Disconnected` (loop ends, handle removed) |

The S3 call itself is **awaited** in the loop, so the detail is a real result rather than
"dispatched". The `s3_response_received` event that follows is raised from its own registered
task: a dashboard-created client defaults to a `*` → manual rule, and awaiting a parked LLM call
inside the loop would wedge it far past the dashboard's 30s send timeout on an action that in
fact succeeded.

Test: `tests/client/s3/command_channel_test.rs` (no LLM, no AWS — a loopback listener serving
canned S3 XML is the endpoint).
