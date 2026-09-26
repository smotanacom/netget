# etcd Client Implementation

## Overview

etcd client for connecting to etcd v3 servers and performing key-value operations under LLM control.

**Status**: Experimental
**Protocol Version**: etcd v3 (gRPC)
**Default Port**: 2379

## Library Choices

### Core Dependencies

- **etcd-client** v0.14 - Official etcd v3 Rust client
    - Provides async gRPC-based KV operations
    - Handles connection management and retries
    - Built on top of tonic (gRPC) and tokio
- **tonic** - gRPC framework (dependency of etcd-client)
- **tokio** - Async runtime

**Rationale**: etcd-client is the official and most mature Rust client for etcd v3, providing a high-level API that
abstracts the complexity of gRPC and protobuf encoding.

## Architecture

### Connection Model

**One session, held for the client's lifetime**:

- `etcd_client::Client::connect` runs once in `connect_with_llm_actions`; a failure there is a
  failed connect.
- The connected client is kept in `SharedEtcd` (`Arc<Mutex<etcd_client::Client>>`) and shared
  by the connected-event handler, the follow-up chain and the injected-command loop, so the
  session the dashboard calls "connected" is the one every operation uses.
- Each operation **clones** the client out of the mutex and runs the RPC on the clone. The
  client is a cheap handle over one tonic channel; holding the guard across the RPC would
  serialise every operation behind the slowest one.

### LLM Integration

**Event Flow**:

1. **Connect** → `etcd_connected` event → LLM generates initial actions
2. **Operation** → Execute get/put/delete → `etcd_response_received` event → LLM decides next action
3. **Disconnect** → Client cleanup

**State Machine**:

- Unlike TCP/Redis, etcd client doesn't use Idle/Processing/Accumulating states
- Operations are synchronous and don't queue
- Each LLM action triggers an immediate etcd operation

### Action System

**Async Actions** (user-triggered):

- `etcd_get` - Retrieve key-value pairs
- `etcd_put` - Store key-value pairs
- `etcd_delete` - Delete keys
- `disconnect` - Close client

**Sync Actions** (response-triggered):

- `etcd_get` - Follow-up get after response
- `etcd_put` - Follow-up put after response

**Custom Action Results**:

- All etcd operations return `ClientActionResult::Custom` with operation-specific data
- Action executor in event handler unpacks custom data and calls appropriate EtcdClient methods

### Event Types

**Defined Events**:

- `etcd_connected` - Client successfully connected to server
    - Parameters: `remote_addr` (string)

- `etcd_response_received` - Operation completed
    - Parameters:
        - `operation` (string) - "get", "put", or "delete"
        - `key` (string) - Key that was operated on
        - `kvs` (array, for get) - Key-value pairs returned
        - `count` (number, for get) - Total matching keys
        - `more` (boolean, for get) - Whether more results exist
        - `revision` (number, for put) - Revision after put
        - `deleted` (number, for delete) - Number of keys deleted

## Implementation Details

### Get Operation

```rust
// LLM action: {"type": "etcd_get", "key": "/config/database"}
etcd_client.get(key, None).await?

// Response includes:
// - kvs: Vec<KeyValue> with key, value, create_revision, mod_revision, version, lease
// - count: Total matching keys
// - more: Whether there are more results (pagination)
```

### Put Operation

```rust
// LLM action: {"type": "etcd_put", "key": "/config/database", "value": "postgres://..."}
etcd_client.put(key, value, None).await?

// Response includes:
// - header: ResponseHeader with cluster_id, member_id, revision
// - prev_kv: Previous key-value (if requested in options)
```

### Delete Operation

```rust
// LLM action: {"type": "etcd_delete", "key": "/config/database"}
etcd_client.delete(key, None).await?

// Response includes:
// - deleted: Number of keys deleted
// - prev_kvs: Deleted key-values (if requested in options)
```

### Options (Future Enhancement)

etcd-client supports rich options that can be exposed to LLM:

- **GetOptions**: `with_prefix()`, `with_range()`, `with_limit()`, `with_sort_order()`
- **PutOptions**: `with_lease()`, `with_prev_kv()`
- **DeleteOptions**: `with_prefix()`, `with_prev_kv()`

Currently using `None` for all options (simple get/put/delete).

## Logging

**Dual Logging**:

- **INFO**: Connection events ("etcd client 1 connected to localhost:2379")
- **DEBUG**: Operation summaries ("etcd client 1 received 3 key-value pairs")
- **ERROR**: Operation failures ("etcd client 1 LLM call failed after get")

Both tracing macros and `status_tx` used for TUI visibility.

## Limitations

### Phase 1 (Current) - Basic KV Operations

**Implemented**:

- ✅ Get - Retrieve single keys
- ✅ Put - Store key-value pairs
- ✅ Delete - Delete keys

**Not Implemented**:

- ❌ Range queries - Get keys with prefix (requires GetOptions)
- ❌ Watch - Real-time change notifications (streaming RPC)
- ❌ Transactions - Compare-and-swap operations
- ❌ Leases - TTL-based expiration
- ❌ Authentication - User/password auth
- ❌ TLS - Encrypted connections

### No Connection Pooling Control

- etcd-client manages connection pool internally
- No explicit control over connection lifecycle

### No Streaming RPCs

- Watch requires bidirectional streaming
- LeaseKeepAlive requires streaming
- Current implementation is unary RPC only (request-response)

## Known Issues

### protoc Binary Dependency

- **Status**: etcd-client v0.15 requires protoc binary at build time
- **Workaround**: Install protoc or download from https://github.com/protocolbuffers/protobuf/releases
- **NetGet Integration**: NetGet's build.rs now uses protox (pure Rust) for etcd server protobuf compilation
- **Limitation**: etcd-client crate's build.rs still requires protoc (cannot be controlled from our build.rs)

**protox Integration Investigation**:

- Successfully integrated protox v0.7 in NetGet's build.rs
- Our etcd server protobuf compilation (proto/etcd/rpc.proto) works WITHOUT protoc binary
- However, etcd-client dependency has its own build.rs that calls tonic-build/prost-build with protoc
- Error when protoc unavailable: "Could not find `protoc`" from etcd-client v0.15.0 build script

**Potential Solutions** (future work):

1. **Fork etcd-client**: Modify its build.rs to use protox instead of requiring protoc
2. **Alternative library**: Use etcd-rs or implement custom gRPC client with protox
3. **Upstream PR**: Submit PR to etcd-client to add protox support
4. **Accept dependency**: Document protoc requirement (current approach)

**Why This Matters**:

- protoc binary is large (~2MB) and platform-specific
- Requires system package manager or manual download
- protox is pure Rust, works anywhere Rust compiles
- Reduces build environment setup complexity

**Current Status**: protoc binary required for building etcd client feature. Install via:

```bash
# Debian/Ubuntu
apt-get install protobuf-compiler

# Or download binary
wget https://github.com/protocolbuffers/protobuf/releases/download/v28.3/protoc-28.3-linux-x86_64.zip
unzip protoc-28.3-linux-x86_64.zip -d $HOME/protoc
export PATH="$HOME/protoc/bin:$PATH"
```

### No Watch Support

- LLM cannot be notified of key changes in real-time
- Must poll with get operations
- **Future**: Implement streaming RPCs for watch

### Error Handling

- etcd errors (key not found, etc.) propagate as Rust errors
- LLM doesn't get structured error events
- **Future**: Add `etcd_error_received` event

## Example Prompts

### Simple Get/Put

```
connect to etcd at localhost:2379
Store configuration: PUT /config/database = "postgres://localhost:5432/mydb"
Retrieve it back with GET /config/database
```

### Service Discovery Simulation

```
connect to etcd at localhost:2379
Register service instances:
  PUT /services/api/instance-1 = "http://10.0.1.5:8080"
  PUT /services/api/instance-2 = "http://10.0.1.6:8080"
Then retrieve /services/api/instance-1 to verify
```

### Configuration Management

```
connect to etcd at localhost:2379
Store application config:
  PUT /app/timeout = "30"
  PUT /app/max_connections = "100"
  PUT /app/log_level = "debug"
Then retrieve /app/timeout
```

## Performance Characteristics

### Latency

- **Connect**: ~50-200ms (one-time handshake)
- **Get/Put/Delete**: ~10-50ms per operation + 2-5s LLM processing
- Total: ~2-5 seconds per LLM-controlled operation

### Throughput

- **Limited by LLM**: ~0.2-0.5 operations per second
- etcd operations are fast (~10-50ms) but LLM processing dominates
- Concurrent clients can operate independently

### Network Efficiency

- gRPC uses HTTP/2 (multiplexed, binary)
- Protobuf encoding is compact

## Security Considerations

### No Authentication (Phase 1)

- Connects to etcd without credentials
- Assumes etcd server is open or on trusted network
- **Future**: Add username/password auth support

### No TLS (Phase 1)

- Plaintext gRPC connection
- Data transmitted unencrypted
- **Future**: Add TLS support with certificate validation

### Endpoint Validation

- Client accepts arbitrary endpoints from user
- No validation of hostname/IP
- Could connect to malicious etcd servers

## Future Enhancements

### Phase 2: Advanced KV Operations

- Range queries (prefix-based get)
- Sorted results
- Pagination with limit
- Previous key-value retrieval

### Phase 3: Transactions

- Compare-and-swap operations
- Atomic multi-key updates
- Distributed locking support

### Phase 4: Streaming

- Watch for key changes
- Real-time notifications to LLM
- Event-driven architecture

### Phase 5: Auth & Security

- Username/password authentication
- TLS encryption
- Certificate validation

## System Dependencies

### macOS Setup

**etcd-client on macOS**: The `etcd-client` v0.15 Rust crate is a pure gRPC client with **no system dependencies** for
normal operation.

**Build**:

```bash
# No special setup needed
./cargo-isolated.sh build --no-default-features --features etcd
```

**Run with etcd server**:

```bash
# Requires running etcd server on localhost:2379
# Start etcd first (if installed):
etcd

# Then in netget:
netget> connect to etcd at localhost:2379
```

**Installing etcd locally** — required: `tests/client/etcd/real_server_test.rs` spawns `etcd` and `etcdctl` and fails without them:

```bash
# Install via Homebrew
brew install etcd

# Or from source:
git clone https://github.com/etcd-io/etcd.git
cd etcd && ./build.sh && ./bin/etcd

# Verify installation
etcd --version
```

### Linux Setup

**Installation**:

```bash
# Debian/Ubuntu - the packaged etcdctl may default to the v2 API (jammy ships 3.3);
# the upstream v3.5 release tarball is what CI installs
sudo apt-get install etcd-server

# Fedora/RHEL
sudo dnf install etcd

# Alpine
apk add etcd

# Arch
sudo pacman -S etcd
```

### Troubleshooting

**"Connection refused" when connecting to etcd**:

- Ensure etcd server is running: `etcd &` or `brew services start etcd`
- Check etcd is listening on port 2379: `lsof -i :2379`
- Try connecting to explicit address: `connect to etcd at 127.0.0.1:2379`

**protoc binary requirement (for etcd-client build)**:

- The etcd-client crate v0.15 requires protoc at build time
- Install protoc: `brew install protobuf`
- Or download binary: https://github.com/protocolbuffers/protobuf/releases

## References

- [etcd-client Documentation](https://docs.rs/etcd-client/)
- [etcd v3 API](https://etcd.io/docs/v3.5/learning/api/)
- [etcd gRPC Protocol](https://github.com/etcd-io/etcd/tree/main/api/etcdserverpb)

## Key Design Principles

1. **Simplicity** - Use high-level etcd-client API, avoid raw gRPC
2. **Stateless** - Reconnect per operation, no connection state management
3. **LLM-Friendly** - Structured actions (get/put/delete) not raw bytes
4. **Incremental** - Start with basic KV, defer advanced features
5. **Observable** - Dual logging for debugging and monitoring

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running etcd client. The handle is
registered **before** the `etcd_connected` LLM call, because a dashboard-created client
defaults to a `*` → manual rule and that call can park for minutes waiting for a human.

**The connection model above is out of date: the session is no longer re-dialled per
operation.** `connect_with_llm_actions` used to build an `etcd_client::Client`, bind it to
`_etcd_client` and drop it on the next line, and every `get`/`put`/`delete` then called
`Client::connect` again. It is now held in an `Arc<Mutex<etcd_client::Client>>` for the life
of the client, which is what makes injected actions possible at all — a command loop that
re-dialled would be a second connection path with its own failure modes and no relationship
to the session the dashboard reports as "connected".

**The connected-event handler used to discard the LLM's actions.** It logged
`"LLM generated N actions on connect"` and returned; nothing executed them, so an instruction
like "PUT /a = b on connect" silently did nothing. It now runs them through the same
`apply_action` the command loop uses.

**Outcome semantics — `Executed`, never `Sent`.** `etcd-client` owns the HTTP/2 connection
and never reports how many bytes a request serialised to, so a byte count here would be
invented. The detail carries what etcd actually answered:
`etcd_put '/k' -> revision 7`, `etcd_get '/k' -> 1 key-value pair(s)`,
`etcd_delete '/k' -> 1 key(s) deleted`. A failed operation is an `Err`, an unknown action is
`Rejected`, and `disconnect` is `Disconnected`.

The `etcd_response_received` event fires from its own registered task rather than inline, so
a manual rule parking that LLM call cannot wedge the command loop.

The old 5-second idle-poll task is gone; the command loop ends when `remove_client` drops the
command sender.
