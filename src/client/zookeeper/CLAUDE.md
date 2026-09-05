# ZooKeeper Client Implementation

## Overview

ZooKeeper client implementation that connects to ZooKeeper servers and allows LLM-controlled operations such as creating znodes, reading/writing data, and watching for changes.

## Implementation

### Library Choices

- **zookeeper-async v0.7**: Modern async Rust ZooKeeper client built on tokio
- **Binary Protocol**: Full ZooKeeper wire protocol support
- **Session Management**: Automatic session keepalive and reconnection

### Architecture

**Connection Flow**:
1. Client connects to ZooKeeper server via TCP (default port 2181)
2. Session established with timeout negotiation
3. LLM controls all operations via actions
4. Watchers can be set up for change notifications

**Operations**:
- `create_znode`: Create a new znode at specified path
- `get_data`: Read data from a znode
- `set_data`: Write data to a znode
- `delete_znode`: Delete a znode
- `get_children`: Get list of child znodes

### LLM Integration

**LLM Control Points**:
- ZNode creation (path, data, flags)
- Data reading and writing
- ZNode deletion
- Children listing
- Watch setup for notifications

**Actions**:
- Async: `create_znode`, `get_data`, `set_data`, `delete_znode`, `get_children`, `modify_instruction`
- Sync: `wait_for_more`, `disconnect`

**Events**:
- `zookeeper_connected`: Client connected successfully
- `zookeeper_data_received`: Data received from getData operation
- `zookeeper_children_received`: Children list received from getChildren

### Logging

**Dual logging** to both `netget.log` and TUI:
- INFO: Connection lifecycle
- DEBUG: Operation summaries (create, get, set, delete)
- TRACE: Full request/response details

## Limitations

1. **Simplified Implementation**: Basic operations only (no transactions, ACLs, or advanced features)
2. **No Watch Mechanism**: Change watches not fully implemented
3. **No Persistent Sessions**: Sessions not preserved across restarts
4. **Synchronous Operations**: Operations are synchronous (no pipelining)

## Example Usage

### Connect and Read Configuration

```
open_client zookeeper localhost:2181

Instruction: "Connect to ZooKeeper and read configuration from /myapp/config.
Log the configuration data."
```

### Service Discovery

```
open_client zookeeper localhost:2181

Instruction: "Monitor service registrations under /services.
List all services by getting children of /services.
For each service, read its data to get the endpoint."
```

### Dynamic Configuration Update

```
open_client zookeeper localhost:2181

Instruction: "Read current configuration from /myapp/timeout.
If timeout is less than 30 seconds, update it to 30.
Verify the update by reading it again."
```

### Hierarchical Data Management

```
open_client zookeeper localhost:2181

Instruction: "Create hierarchical structure:
/myapp (container)
/myapp/config (data: 'prod')
/myapp/services (container)

Then list all children under /myapp to verify."
```

## Testing Strategy

See `tests/client/zookeeper/CLAUDE.md` for E2E testing approach.

## Command channel (the dashboard's `[ send ]`) — registered, but it cannot act

`AppState::send_to_client` will accept an action for a running ZooKeeper client, and the
handle is registered before anything that could park. **What it can do is bounded by the
client itself, which does not connect.**

Read `connect_with_llm_actions` before trusting anything above in this file: it parses the
address, marks the client `Connected`, and returns. It creates no
`zookeeper_async::ZooKeeper`, registers no watcher, raises no event and never calls the LLM —
the read loop in the original file was commented out (`// loop {`). So none of
`create_znode` / `get_data` / `set_data` / `delete_znode` / `get_children` has ever run
against a server, and the events listed above have never fired.

The command loop therefore reports:

- `Rejected { error }` for an action the protocol itself refuses — this is real, and comes
  from the client's own `execute_action`, so parameter validation genuinely works.
- `Disconnected` for `disconnect`, which really does end the loop, set the status and drop
  the handle.
- `Executed { detail: "'create_znode' was validated but not performed: the ZooKeeper client
  establishes no zookeeper-async session (connect_with_llm_actions is a placeholder), so
  there is nothing to run it against" }` for every operation verb.

That last one is the point of registering at all: the alternative — leaving the client
without a channel — tells the operator only "this client has no command channel yet", which
reads as "not implemented yet" rather than "this client is not connected to anything".
A fabricated `Sent { bytes_sent: 0 }` would be worse than either.

**When the session is implemented**, the loop is where injected actions should be applied,
through the same `apply_action` the LLM path will use — copy `src/client/redis/mod.rs`. The
session handle must live behind an `Arc<Mutex<_>>` reachable from both, exactly as
`src/client/etcd/mod.rs` now does with its `etcd_client::Client`.

There is no `tests/client/zookeeper/` directory and no `pub mod zookeeper;` in
`tests/client/mod.rs`, so this behaviour is currently unguarded by any test.
