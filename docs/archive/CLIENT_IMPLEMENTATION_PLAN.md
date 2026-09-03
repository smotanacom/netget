# NetGet Client Implementation Plan

## Executive Summary

This document outlines the plan to add **LLM-controlled client capability** to NetGet. Currently, NetGet runs servers where an LLM responds to incoming connections. This plan adds the inverse: clients where an LLM initiates and controls outbound connections to remote servers.

**Key Design Principle**: Mirror the existing server architecture patterns to maintain consistency and leverage existing infrastructure.

---

## Architecture Overview

### Current Server Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         AppState                            │
│  ┌────────────────────────────────────────────────────────┐ │
│  │  servers: HashMap<ServerId, ServerInstance>            │ │
│  │  mode: Mode (Server/Client/Idle)                       │ │
│  │  tasks: HashMap<TaskId, ScheduledTask>                 │ │
│  └────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
         │
         ├─► ServerInstance (per server)
         │   ├─ connections: HashMap<ConnectionId, ConnectionState>
         │   ├─ instruction: String (LLM instructions)
         │   ├─ memory: String (LLM memory)
         │   └─ protocol_data: JSON (flexible storage)
         │
         └─► Protocol Implementation (Server trait)
             ├─ spawn() - Start listening
             ├─ get_async_actions() - User-triggered actions
             ├─ get_sync_actions() - Network-event actions
             └─ execute_action() - Action executor
```

### Proposed Client Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         AppState                            │
│  ┌────────────────────────────────────────────────────────┐ │
│  │  servers: HashMap<ServerId, ServerInstance>            │ │
│  │  clients: HashMap<ClientId, ClientInstance>  ← NEW     │ │
│  │  mode: Mode (Server/Client/Idle)                       │ │
│  │  tasks: HashMap<TaskId, ScheduledTask>                 │ │
│  └────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
         │
         ├─► ClientInstance (per client)  ← NEW
         │   ├─ remote_addr: String (target server)
         │   ├─ connection_state: ClientConnectionState
         │   ├─ instruction: String (LLM instructions)
         │   ├─ memory: String (LLM memory)
         │   └─ protocol_data: JSON (flexible storage)
         │
         └─► Protocol Implementation (Client trait)  ← NEW
             ├─ connect() - Initiate connection
             ├─ get_async_actions() - User-triggered actions
             ├─ get_sync_actions() - Connection-event actions
             └─ execute_action() - Action executor
```

---

## Core Components

### 1. State Management (`src/state/client.rs`) ← NEW FILE

Mirror `src/state/server.rs` with client-specific types:

```rust
/// Unique identifier for a client instance
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u32);

impl ClientId {
    pub fn new(id: u32) -> Self { Self(id) }
    pub fn as_u32(&self) -> u32 { self.0 }
    pub fn from_string(s: &str) -> Option<Self> { /* "client-123" */ }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "client-{}", self.0)
    }
}

/// Status of a client connection
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientStatus {
    /// Client is connecting
    Connecting,
    /// Client is connected and active
    Connected,
    /// Client connection has been closed
    Disconnected,
    /// Client encountered an error
    Error(String),
}

/// Connection state for client (Idle/Processing/Accumulating)
/// Reuse ProtocolState from server.rs

/// Client connection state
#[derive(Debug, Clone)]
pub struct ClientConnectionState {
    /// Client ID
    pub id: ClientId,
    /// Remote server address (hostname:port or IP:port)
    pub remote_addr: String,
    /// Actual connected socket address (after DNS resolution)
    pub connected_addr: Option<SocketAddr>,
    /// Local address (our side of the connection)
    pub local_addr: Option<SocketAddr>,
    /// Bytes sent
    pub bytes_sent: u64,
    /// Bytes received
    pub bytes_received: u64,
    /// Packets sent
    pub packets_sent: u64,
    /// Packets received
    pub packets_received: u64,
    /// Last activity timestamp
    pub last_activity: Instant,
    /// Connection status (Connecting/Connected/Disconnected)
    pub status: ClientStatus,
    /// When status last changed
    pub status_changed_at: Instant,
    /// Protocol-specific information (flexible JSON storage)
    pub protocol_info: ProtocolConnectionInfo,
}

/// A client instance with its own connection, state, and configuration
#[derive(Debug)]
pub struct ClientInstance {
    /// Unique client ID
    pub id: ClientId,
    /// Remote server address (hostname:port or IP:port)
    pub remote_addr: String,
    /// Protocol name (e.g., "HTTP", "SSH", "Redis")
    pub protocol_name: String,
    /// User instructions for this client
    pub instruction: String,
    /// LLM memory for this client
    pub memory: String,
    /// Client connection status
    pub status: ClientStatus,
    /// Connection state (if connected)
    pub connection: Option<ClientConnectionState>,
    /// Client task handle (for cleanup)
    pub handle: Option<JoinHandle<()>>,
    /// When the client was created
    pub created_at: Instant,
    /// When the client status last changed
    pub status_changed_at: Instant,
    /// Protocol-specific startup parameters
    pub startup_params: Option<serde_json::Value>,
    /// Script configuration for handling protocol events
    pub script_config: Option<crate::scripting::ScriptConfig>,
    /// Protocol-specific client data (flexible storage)
    pub protocol_data: serde_json::Value,
    /// Log file paths (output_name -> log_file_path)
    pub log_files: HashMap<String, PathBuf>,
}

impl ClientInstance {
    pub fn new(id: ClientId, remote_addr: String, protocol_name: String, instruction: String) -> Self {
        let now = Instant::now();
        Self {
            id,
            remote_addr,
            protocol_name,
            instruction,
            memory: String::new(),
            status: ClientStatus::Connecting,
            connection: None,
            handle: None,
            created_at: now,
            status_changed_at: now,
            startup_params: None,
            script_config: None,
            protocol_data: serde_json::Value::Object(serde_json::Map::new()),
            log_files: HashMap::new(),
        }
    }

    // Mirror methods from ServerInstance:
    // - get_protocol_data(), set_protocol_data()
    // - get_or_create_log_path()
    // - summary()
}
```

**Design Notes:**
- Reuse `ProtocolConnectionInfo` from server.rs (flexible JSON storage)
- Reuse `ProtocolState` from server.rs (Idle/Processing/Accumulating state machine)
- Client has ONE connection (unlike server which has many)
- Client can reconnect: status goes Disconnected → Connecting → Connected

---

### 2. Client Trait (`src/llm/actions/client_trait.rs`) ← NEW FILE

Mirror `protocol_trait.rs` with client-specific behavior:

```rust
/// Trait for protocol client implementations
///
/// Each protocol implements this trait to provide:
/// 1. Client connection - how to connect to a remote server
/// 2. Startup parameters - configuration accepted when connecting
/// 3. Async actions - executable anytime from user input
/// 4. Sync actions - executable during connection events
/// 5. Action executor - parses and executes client actions
/// 6. Protocol metadata - stack name, keywords
pub trait Client: Send + Sync {
    /// Connect to a remote server for this protocol
    ///
    /// This is called when a client needs to be started. The implementation
    /// should connect to the remote address, set up any necessary resources,
    /// and return the connected socket address.
    ///
    /// # Arguments
    /// * `ctx` - Connect context with all necessary dependencies
    ///
    /// # Returns
    /// * `Ok(SocketAddr)` - The actual local address of the connection
    /// * `Err(_)` - If connection failed
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    >;

    /// Get startup parameters that can be provided when connecting
    ///
    /// These parameters configure the client before connecting. Examples:
    /// - HTTP: request_headers, user_agent, follow_redirects
    /// - SSH: username, password, private_key_path
    /// - MySQL: username, password, database
    ///
    /// Default implementation returns empty vector (no startup parameters).
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        Vec::new()
    }

    /// Get async actions that can be executed anytime from user input
    ///
    /// These actions don't require network context. Examples:
    /// - HTTP: send_request(method, path, headers, body)
    /// - Redis: execute_command(cmd, args)
    /// - SSH: send_command(cmd)
    /// - Generic: disconnect(), reconnect()
    fn get_async_actions(&self, state: &AppState) -> Vec<ActionDefinition>;

    /// Get sync actions available during connection events
    ///
    /// These actions only make sense in response to connection events. Examples:
    /// - TCP: send_data(output), wait_for_more()
    /// - HTTP: handle_response_header(), handle_response_body()
    /// - SSH: handle_auth_challenge(), handle_command_output()
    fn get_sync_actions(&self) -> Vec<ActionDefinition>;

    /// Execute a protocol-specific action
    ///
    /// # Arguments
    /// * `action` - The action JSON object from LLM
    ///
    /// # Returns
    /// * `Ok(ClientActionResult)` - Result of execution
    /// * `Err(_)` - If action execution failed
    fn execute_action(&self, action: serde_json::Value) -> Result<ClientActionResult>;

    /// Get protocol name for debugging
    fn protocol_name(&self) -> &'static str;

    /// Get the event types that this client can emit
    ///
    /// Each event type includes:
    /// - A unique ID (e.g., "http_response", "ssh_connected")
    /// - A description of when it occurs
    /// - The actions that can be used to respond to this event
    fn get_event_types(&self) -> Vec<crate::protocol::EventType> {
        Vec::new()
    }

    /// Get the stack name (e.g., "ETH>IP>TCP>HTTP")
    fn stack_name(&self) -> &'static str;

    /// Get parsing keywords for protocol detection
    fn keywords(&self) -> Vec<&'static str>;

    /// Get protocol metadata with implementation details
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2;

    /// Get a short description of this protocol client
    fn description(&self) -> &'static str;

    /// Get an example prompt that would trigger this client
    fn example_prompt(&self) -> &'static str;

    /// Get the group name for categorizing this client protocol
    fn group_name(&self) -> &'static str;
}

/// Result of executing a client action
#[derive(Debug)]
pub enum ClientActionResult {
    /// Data to send to the server
    SendData(Vec<u8>),

    /// Disconnect from the server
    Disconnect,

    /// Wait for more data before responding
    WaitForMore,

    /// No action needed (e.g., logging, state update)
    NoAction,

    /// Multiple results
    Multiple(Vec<ClientActionResult>),

    /// Custom protocol-specific result with structured data
    Custom {
        name: String,
        data: serde_json::Value,
    },
}
```

**Design Notes:**
- `connect()` instead of `spawn()` (client initiates, server listens)
- `ClientActionResult` mirrors `ActionResult` but for client behavior
- Same flexible JSON storage pattern as servers
- Same async/sync action split as servers

---

### 3. Protocol Connect Context (`src/protocol/mod.rs`) ← MODIFY

Add new context struct for client connections:

```rust
/// Context passed to Client::connect()
pub struct ConnectContext {
    /// Remote server address (hostname:port or IP:port)
    pub remote_addr: String,
    /// LLM client for making calls
    pub llm_client: Arc<crate::llm::LlmClient>,
    /// Application state
    pub state: AppState,
    /// Status channel for TUI updates
    pub status_tx: mpsc::UnboundedSender<crate::cli::StatusMessage>,
    /// Client ID for this connection
    pub client_id: ClientId,
    /// Startup parameters (from open_client action)
    pub startup_params: Option<serde_json::Value>,
}
```

---

### 4. Client Registry (`src/protocol/client_registry.rs`) ← NEW FILE

Mirror `src/protocol/registry.rs` but for clients:

```rust
use crate::llm::actions::client_trait::Client;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Global registry of client protocol implementations
pub static CLIENT_REGISTRY: LazyLock<ClientProtocolRegistry> = LazyLock::new(|| {
    let mut registry = ClientProtocolRegistry::new();

    // Register clients
    #[cfg(feature = "tcp")]
    registry.register("tcp", Arc::new(crate::client::tcp::TcpClientProtocol::new()));

    #[cfg(feature = "http")]
    registry.register("http", Arc::new(crate::client::http::HttpClientProtocol::new()));

    #[cfg(feature = "redis")]
    registry.register("redis", Arc::new(crate::client::redis::RedisClientProtocol::new()));

    // ... more clients

    registry
});

pub struct ClientProtocolRegistry {
    protocols: HashMap<String, Arc<dyn Client>>,
}

impl ClientProtocolRegistry {
    pub fn new() -> Self {
        Self {
            protocols: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, client: Arc<dyn Client>) {
        self.protocols.insert(name.to_lowercase(), client);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Client>> {
        self.protocols.get(&name.to_lowercase()).cloned()
    }

    pub fn list(&self) -> Vec<String> {
        self.protocols.keys().cloned().collect()
    }
}
```

**Design Notes:**
- Separate registry from server registry (decentralized pattern)
- Feature-gated registration (same as servers)
- Same lazy initialization pattern

---

### 5. AppState Extensions (`src/state/app_state.rs`) ← MODIFY

Add client management to AppState:

```rust
struct AppStateInner {
    mode: Mode,
    servers: HashMap<ServerId, ServerInstance>,
    clients: HashMap<ClientId, ClientInstance>,  // ← NEW
    next_server_id: u32,
    next_client_id: u32,  // ← NEW
    // ... rest unchanged
}

impl AppState {
    // Client management methods (mirror server methods)

    /// Add a new client instance and return its ID
    pub async fn add_client(&self, mut client: ClientInstance) -> ClientId {
        let mut inner = self.inner.write().await;
        let id = ClientId::new(inner.next_client_id);
        inner.next_client_id += 1;
        client.id = id;
        inner.clients.insert(id, client);

        // Set mode to Client if this is the first client (and no servers)
        if inner.mode == Mode::Idle {
            inner.mode = Mode::Client;
        }

        id
    }

    pub async fn remove_client(&self, id: ClientId) -> Option<ClientInstance> {
        let mut inner = self.inner.write().await;
        let client = inner.clients.remove(&id);

        // Set mode to Idle if no more clients and no servers
        if inner.clients.is_empty() && inner.servers.is_empty() {
            inner.mode = Mode::Idle;
        }

        client
    }

    pub async fn get_client(&self, id: ClientId) -> Option<ClientInstance> { /* ... */ }
    pub async fn get_all_client_ids(&self) -> Vec<ClientId> { /* ... */ }
    pub async fn get_all_clients(&self) -> Vec<ClientInstance> { /* ... */ }
    pub async fn update_client_status(&self, id: ClientId, status: ClientStatus) { /* ... */ }
    pub async fn get_instruction_for_client(&self, client_id: ClientId) -> Option<String> { /* ... */ }
    pub async fn set_instruction_for_client(&self, client_id: ClientId, instruction: String) { /* ... */ }
    pub async fn get_memory_for_client(&self, client_id: ClientId) -> Option<String> { /* ... */ }
    pub async fn set_memory_for_client(&self, client_id: ClientId, memory: String) { /* ... */ }
    pub async fn append_memory_for_client(&self, client_id: ClientId, text: String) { /* ... */ }

    // ... all the same patterns as servers
}
```

**Design Notes:**
- Mode can be Server OR Client OR Idle
- If both servers and clients exist, mode stays as whichever was set first (or we could add a Mixed mode)
- Same RwLock patterns as servers (never hold during I/O)

---

### 6. Common Actions Extensions (`src/llm/actions/common.rs`) ← MODIFY

Add client-related common actions:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CommonAction {
    // Existing server actions
    #[serde(rename = "open_server")]
    OpenServer { /* ... */ },

    // NEW: Client actions
    #[serde(rename = "open_client")]
    OpenClient {
        protocol: String,
        remote_addr: String,  // "example.com:80" or "192.168.1.1:6379"
        #[serde(default)]
        startup_params: Option<serde_json::Value>,
        #[serde(default)]
        scheduled_tasks: Vec<serde_json::Value>,
    },

    #[serde(rename = "close_client")]
    CloseClient {
        client_id: String,  // "client-1" or "1"
    },

    #[serde(rename = "close_all_clients")]
    CloseAllClients,

    #[serde(rename = "reconnect_client")]
    ReconnectClient {
        client_id: String,
    },

    #[serde(rename = "update_client_instruction")]
    UpdateClientInstruction {
        client_id: String,
        instruction: String,
    },

    // Existing actions
    // ...
}
```

**Design Notes:**
- `remote_addr` is string to allow hostnames ("redis.example.com:6379")
- DNS resolution happens in client implementation
- Same startup_params pattern as servers
- Same scheduled_tasks pattern as servers

---

### 7. CLI Commands (`src/cli/commands.rs`) ← MODIFY

Add client-related user commands:

```rust
#[derive(Debug, Clone)]
pub enum UserCommand {
    // Existing server commands
    OpenServer { /* ... */ },
    CloseServer { /* ... */ },
    ListServers,

    // NEW: Client commands
    OpenClient {
        protocol: String,
        remote_addr: String,
        instruction: Option<String>,
    },
    CloseClient {
        client_id: String,
    },
    ReconnectClient {
        client_id: String,
    },
    ListClients,

    // Existing commands
    // ...
}
```

**Design Notes:**
- Parser needs to distinguish "open http server on port 8080" vs "connect to http at example.com:80"
- Keywords: "connect", "connect to", "open client", "client connection"
- List command shows all active clients with status

---

### 8. Event System Extensions (`src/events.rs`) ← MODIFY

Add client-related events:

```rust
pub enum NetworkEvent {
    // Existing server events
    ConnectionAccepted { /* ... */ },
    DataReceived { /* ... */ },
    ConnectionClosed { /* ... */ },

    // NEW: Client events
    ClientConnected {
        client_id: ClientId,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
    },
    ClientDataReceived {
        client_id: ClientId,
        data: Vec<u8>,
    },
    ClientDisconnected {
        client_id: ClientId,
        reason: String,
    },
    ClientConnectionFailed {
        client_id: ClientId,
        error: String,
    },
}
```

**Design Notes:**
- Client events flow same as server events
- LLM gets notified of connection, data, disconnection
- Same state machine (Idle/Processing/Accumulating)

---

### 9. TUI Updates (`src/cli/rolling_tui.rs`) ← MODIFY

Add client display to TUI:

```
┌────────────────────────────────────────────────────────────┐
│ NetGet - LLM-Controlled Network Protocol Tool             │
├────────────────────────────────────────────────────────────┤
│ Mode: Client | Model: qwen3-coder:30b                     │
├────────────────────────────────────────────────────────────┤
│ SERVERS (0)                                                │
│                                                            │
│ CLIENTS (2)                                                │
│ #1 HTTP → example.com:80 (Connected) - 45s                │
│    Instruction: Fetch /api/status every 10s               │
│    Sent: 1.2 KB | Received: 8.4 KB                        │
│                                                            │
│ #2 Redis → localhost:6379 (Connected) - 2m                │
│    Instruction: Monitor SET commands                       │
│    Sent: 256 B | Received: 128 B                          │
├────────────────────────────────────────────────────────────┤
│ [Logs...]                                                  │
└────────────────────────────────────────────────────────────┘
```

**Design Notes:**
- Show both servers and clients in TUI
- Client shows: ID, protocol, remote_addr, status, duration, stats
- Same sticky footer, same log levels, same controls

---

## Module Structure

```
src/
├─ client/                        ← NEW DIRECTORY
│  ├─ mod.rs                      (client connection helpers)
│  ├─ tcp/
│  │  ├─ mod.rs                   (TCP client implementation)
│  │  ├─ actions.rs               (TcpClientProtocol)
│  │  └─ CLAUDE.md                (implementation notes)
│  ├─ http/
│  │  ├─ mod.rs                   (HTTP client with reqwest)
│  │  ├─ actions.rs               (HttpClientProtocol)
│  │  └─ CLAUDE.md
│  ├─ redis/
│  │  ├─ mod.rs                   (Redis client)
│  │  ├─ actions.rs               (RedisClientProtocol)
│  │  └─ CLAUDE.md
│  └─ ...
│
├─ state/
│  ├─ app_state.rs                (add client management) ← MODIFY
│  ├─ server.rs                   (existing)
│  └─ client.rs                   (ClientInstance, ClientId, etc) ← NEW
│
├─ llm/actions/
│  ├─ protocol_trait.rs           (Server trait) ← existing
│  ├─ client_trait.rs             (Client trait) ← NEW
│  ├─ common.rs                   (add client actions) ← MODIFY
│  └─ executor.rs                 (add client action execution) ← MODIFY
│
├─ protocol/
│  ├─ mod.rs                      (add ConnectContext) ← MODIFY
│  ├─ registry.rs                 (server registry) ← existing
│  └─ client_registry.rs          (client registry) ← NEW
│
├─ cli/
│  ├─ commands.rs                 (add client commands) ← MODIFY
│  ├─ rolling_tui.rs              (add client display) ← MODIFY
│  └─ client_startup.rs           (client startup logic) ← NEW
│
└─ events.rs                      (add client events) ← MODIFY
```

---

## Implementation Phases

### Phase 1: Core Infrastructure (Foundation)

**Goal**: Set up client state management and trait system

**Tasks**:
1. Create `src/state/client.rs` with `ClientInstance`, `ClientId`, `ClientStatus`
2. Create `src/llm/actions/client_trait.rs` with `Client` trait and `ClientActionResult`
3. Modify `src/state/app_state.rs` to add client management methods
4. Create `src/protocol/client_registry.rs` with `CLIENT_REGISTRY`
5. Modify `src/protocol/mod.rs` to add `ConnectContext`
6. Modify `src/llm/actions/common.rs` to add client actions
7. Create `src/client/mod.rs` directory and base module

**Validation**: Code compiles, client state can be created and managed

**Files Created**: 3 new files, 3 modified files
**Estimated Lines**: ~800 lines

---

### Phase 2: CLI Integration

**Goal**: Allow users to request client connections

**Tasks**:
1. Modify `src/cli/commands.rs` to add `OpenClient`, `CloseClient`, `ListClients` commands
2. Create `src/cli/client_startup.rs` for client connection logic
3. Modify `src/cli/rolling_tui.rs` to display clients in UI
4. Modify parser to recognize client keywords ("connect", "connect to")
5. Add client-related status messages

**Validation**: User can type "connect to http at example.com:80" and see client in UI

**Files Created**: 1 new file, 3 modified files
**Estimated Lines**: ~400 lines

---

### Phase 3: TCP Client Implementation (First Protocol)

**Goal**: Create a working TCP client as reference implementation

**Tasks**:
1. Create `src/client/tcp/mod.rs` with TCP client connection logic
2. Create `src/client/tcp/actions.rs` with `TcpClientProtocol` implementing `Client` trait
3. Create `src/client/tcp/CLAUDE.md` documenting implementation
4. Register TCP client in `CLIENT_REGISTRY`
5. Add feature gate `tcp` to `Cargo.toml` (reuse existing feature)
6. Implement actions: `send_data`, `disconnect`, `wait_for_more`
7. Add dual logging (tracing + status_tx)
8. Implement state machine (Idle/Processing/Accumulating)

**Validation**: Can connect to TCP server, send/receive data, LLM controls behavior

**Files Created**: 3 new files, 2 modified files
**Estimated Lines**: ~500 lines

---

### Phase 4: HTTP Client Implementation

**Goal**: Create HTTP client with reqwest

**Tasks**:
1. Create `src/client/http/mod.rs` with HTTP client using reqwest
2. Create `src/client/http/actions.rs` with `HttpClientProtocol`
3. Create `src/client/http/CLAUDE.md`
4. Implement actions: `send_request`, `handle_response`, `disconnect`
5. Add request/response structured data (no raw bytes in actions!)
6. Register in `CLIENT_REGISTRY`

**Validation**: Can make HTTP requests to remote servers, LLM constructs requests

**Files Created**: 3 new files, 1 modified file
**Estimated Lines**: ~600 lines

---

### Phase 5: Redis Client Implementation

**Goal**: Create Redis client as example of stateful protocol

**Tasks**:
1. Create `src/client/redis/mod.rs` with Redis client
2. Create `src/client/redis/actions.rs` with `RedisClientProtocol`
3. Create `src/client/redis/CLAUDE.md`
4. Implement actions: `execute_command`, `handle_response`, `disconnect`
5. Use structured actions (command, args) not raw RESP bytes

**Validation**: Can connect to Redis, execute commands, LLM interprets responses

**Files Created**: 3 new files, 1 modified file
**Estimated Lines**: ~500 lines

---

### Phase 6: Testing Infrastructure

**Goal**: Add E2E tests for clients

**Tasks**:
1. Create `tests/client/tcp/e2e_test.rs` with TCP client tests
2. Create `tests/client/tcp/CLAUDE.md` documenting test strategy
3. Create `tests/client/http/e2e_test.rs` with HTTP client tests
4. Create `tests/client/http/CLAUDE.md`
5. Create `tests/client/redis/e2e_test.rs` with Redis client tests
6. Create `tests/client/redis/CLAUDE.md`
7. Update `tests/client/helpers.rs` with client test utilities
8. Ensure all tests are feature-gated
9. Ensure tests use an in-process mock LLM for concurrency safety
10. Keep LLM call count < 10 per test suite

**Validation**: All client tests pass, documented, < 10 LLM calls per suite

**Files Created**: 9 new files
**Estimated Lines**: ~1200 lines

---

### Phase 7: Additional Clients (Optional)

Implement clients for other protocols as needed:
- **SSH Client**: ssh2-rs crate, execute commands, handle output
- **MySQL Client**: mysql_async crate, execute queries, handle results
- **PostgreSQL Client**: tokio-postgres crate
- **WebSocket Client**: tokio-tungstenite crate
- **DNS Client**: trust-dns-resolver crate
- **MQTT Client**: rumqttc crate

Each follows same pattern as Phase 3-5.

---

## Key Design Decisions

### 1. One Connection Per Client

**Decision**: Each `ClientInstance` has ONE connection (unlike servers with many)

**Rationale**:
- Clients initiate, servers accept
- Simpler state management
- User creates multiple clients for multiple connections
- Matches mental model: "connect to server X" = 1 client instance

**Alternative Considered**: Connection pool per client (rejected: too complex for v1)

---

### 2. Mode Handling

**Decision**: Mode can be Server, Client, or Idle. If both servers and clients exist, mode reflects whichever was created first.

**Rationale**:
- Simple to implement
- Matches current pattern
- User rarely needs both simultaneously (different use cases)

**Alternative Considered**: Add Mode::Mixed (rejected: adds complexity, rare use case)

---

### 3. Separate Client Trait

**Decision**: Create separate `Client` trait, don't overload `Server` trait

**Rationale**:
- Clear separation of concerns
- Server and Client have different lifecycles (listen vs connect)
- Different action semantics (accept vs initiate)
- Avoid conditional logic based on mode

**Alternative Considered**: Single Protocol trait with mode parameter (rejected: messy)

---

### 4. Structured Actions (CRITICAL)

**Decision**: Client actions use structured data, NEVER raw bytes

**Example**:
```json
// GOOD: HTTP request action
{
  "type": "send_http_request",
  "method": "GET",
  "path": "/api/status",
  "headers": {"User-Agent": "NetGet/1.0"},
  "body": null
}

// BAD: Raw bytes
{
  "type": "send_data",
  "data": "R0VUIC9hcGkvc3RhdHVzIEhUVFAvMS4xXHJcbg=="
}
```

**Rationale**:
- LLMs understand structured data
- LLMs cannot construct binary protocols
- Protocol libraries handle serialization
- Matches Actions/Events Design principle from CLAUDE.md

---

### 5. DNS Resolution

**Decision**: Client implementation handles DNS resolution, not framework

**Rationale**:
- Different protocols may need different resolution (A vs AAAA vs SRV)
- Protocol-specific timeout/retry logic
- Keeps framework simple

---

### 6. Reconnection Logic

**Decision**: User/LLM must explicitly trigger reconnection via `reconnect_client` action

**Rationale**:
- Explicit control for LLM
- Prevents infinite loops
- User can implement retry logic via scheduled tasks

**Alternative Considered**: Automatic reconnection (rejected: reduces LLM control)

---

### 7. Feature Gating

**Decision**: Reuse existing server feature flags for clients (e.g., `feature = "http"` enables both server and client)

**Rationale**:
- Simplifies Cargo.toml
- Users typically want both server and client for same protocol
- Reduces feature explosion

**Alternative Considered**: Separate features like `http-client`, `http-server` (rejected: too granular)

---

## Common Pitfalls to Avoid

Based on CLAUDE.md guidance:

### 1. ❌ Centralized Enum Fighting

**Bad**: Adding `ClientProtocol::Http | Tcp | Redis` enum

**Good**: Use trait-based registry pattern with flexible JSON storage

---

### 2. ❌ Missing Feature Gates

**Bad**: Unconditional client registration in `client_registry.rs`

**Good**: `#[cfg(feature = "http")]` for every protocol registration

---

### 3. ❌ Holding Mutex During I/O

**Bad**:
```rust
let state = app_state.lock().unwrap();
stream.write_all(&data).await?;  // DEADLOCK RISK
```

**Good**:
```rust
let data = {
    let state = app_state.lock().unwrap();
    state.get_data()
};
stream.write_all(&data).await?;
```

---

### 4. ❌ Raw Bytes in Actions

**Bad**:
```json
{"type": "send_data", "data": "SGVsbG8gV29ybGQ="}
```

**Good**:
```json
{"type": "send_http_request", "method": "GET", "path": "/"}
```

---

### 5. ❌ Missing CLAUDE.md Files

Every client protocol MUST have:
- `src/client/<protocol>/CLAUDE.md` (implementation notes)
- `tests/client/<protocol>/CLAUDE.md` (test strategy)

---

### 6. ❌ Inefficient E2E Tests

**Bad**: 50 LLM calls per test suite (slow, expensive)

**Good**: < 10 LLM calls per suite (reuse connections, bundle scenarios)

---

### 7. ❌ Forgetting Dual Logging

Every client MUST log to:
- Tracing macros (`debug!`, `info!`, `warn!`, `error!`)
- Status channel (`status_tx.send()`)

---

## Testing Strategy

### Unit Tests

**Location**: `tests/client/`

**Coverage**:
- Client state management (create, update, remove)
- Action parsing and execution
- Status transitions (Connecting → Connected → Disconnected)
- Feature gate validation

**LLM Calls**: 0 (pure unit tests)

---

### E2E Tests

**Location**: `tests/client/<protocol>/e2e_test.rs`

**Strategy**:
- Use real servers (spawn with Docker or local services)
- TCP: nc or custom echo server
- HTTP: httpbin.org or local Python server
- Redis: Docker container
- Test client connection, data exchange, disconnection
- Validate LLM controls client behavior

**Budget**: < 10 LLM calls per protocol

**Example**:
```rust
#[cfg(all(test, feature = "http"))]
#[tokio::test]
async fn test_http_client_get_request() {
    // Spawn local HTTP server
    // Connect client
    // LLM constructs GET request
    // Validate response
    // < 10 LLM calls total
}
```

---

## Documentation Requirements

### Per-Client Documentation

Each client protocol MUST have:

**`src/client/<protocol>/CLAUDE.md`**:
- Library choice and rationale
- Connection lifecycle
- LLM control points
- Action descriptions
- Limitations
- Example prompts

**`tests/client/<protocol>/CLAUDE.md`**:
- Test strategy
- LLM call budget
- Runtime expectations
- Known issues
- Server setup (Docker, local, etc.)

---

### Updated Root Documentation

**`CLAUDE.md`**: Add client section describing:
- Client architecture
- Supported client protocols
- Example prompts
- Testing strategy

**`CLIENT_ARCHITECTURE.md`** (NEW): Technical deep-dive on client implementation

---

## Rollout Plan

### Week 1: Foundation
- Phase 1 (Core Infrastructure)
- Phase 2 (CLI Integration)

### Week 2: First Clients
- Phase 3 (TCP Client)
- Phase 4 (HTTP Client)

### Week 3: Database Client & Testing
- Phase 5 (Redis Client)
- Phase 6 (Testing Infrastructure)

### Week 4+: Expansion
- Phase 7 (Additional Clients)
- Documentation polish
- Performance optimization

---

## Success Criteria

✅ **Core Functionality**:
- User can connect clients via CLI
- LLM controls client behavior
- Clients shown in TUI
- State management works

✅ **Protocol Coverage**:
- TCP client (baseline)
- HTTP client (structured actions)
- Redis client (stateful protocol)

✅ **Testing**:
- E2E tests pass
- < 10 LLM calls per suite
- Feature gates validated

✅ **Documentation**:
- All CLAUDE.md files present
- Client architecture documented
- Example prompts working

✅ **Code Quality**:
- No centralized enums
- Dual logging everywhere
- No mutex-holding during I/O
- Feature gates on all protocols

---

## Open Questions

### 1. Connection Pooling?

**Question**: Should clients support connection pooling (multiple connections per client)?

**Current Answer**: No, keep v1 simple. User creates multiple clients if needed.

**Future**: Could add `ConnectionPool` mode in v2 if demand exists.

---

### 2. Mode::Mixed?

**Question**: Should we add Mode::Mixed for running servers and clients simultaneously?

**Current Answer**: No, mode reflects first-created type. Rare use case.

**Future**: Could add if users request it.

---

### 3. Client Discovery?

**Question**: Should clients support service discovery (mDNS, Consul, etc.)?

**Current Answer**: No, user provides explicit remote_addr. Discovery can be external.

**Future**: Could add discovery_params to startup_params if needed.

---

### 4. TLS/SSL?

**Question**: How to handle TLS client connections?

**Current Answer**: Protocol-specific. HTTP client uses reqwest with TLS. TCP client could have `use_tls` startup param.

**Future**: Common TLS config abstraction if many protocols need it.

---

## Risk Mitigation

### Risk 1: Breaking Server Functionality

**Mitigation**:
- Extensive testing before merge
- Feature flags for clients
- Server tests continue passing

---

### Risk 2: Complexity Explosion

**Mitigation**:
- Start with 3 clients (TCP, HTTP, Redis)
- Validate architecture before expanding
- Reuse patterns from servers

---

### Risk 3: LLM Confusion (Server vs Client)

**Mitigation**:
- Clear keywords ("connect" vs "open server")
- Mode displayed prominently in TUI
- Separate action namespaces

---

## Appendix: File Checklist

### New Files (Estimated 28 files)
- [ ] `src/state/client.rs`
- [ ] `src/llm/actions/client_trait.rs`
- [ ] `src/protocol/client_registry.rs`
- [ ] `src/cli/client_startup.rs`
- [ ] `src/client/mod.rs`
- [ ] `src/client/tcp/mod.rs`
- [ ] `src/client/tcp/actions.rs`
- [ ] `src/client/tcp/CLAUDE.md`
- [ ] `src/client/http/mod.rs`
- [ ] `src/client/http/actions.rs`
- [ ] `src/client/http/CLAUDE.md`
- [ ] `src/client/redis/mod.rs`
- [ ] `src/client/redis/actions.rs`
- [ ] `src/client/redis/CLAUDE.md`
- [ ] `tests/client/tcp/e2e_test.rs`
- [ ] `tests/client/tcp/CLAUDE.md`
- [ ] `tests/client/http/e2e_test.rs`
- [ ] `tests/client/http/CLAUDE.md`
- [ ] `tests/client/redis/e2e_test.rs`
- [ ] `tests/client/redis/CLAUDE.md`
- [ ] `tests/client/helpers.rs`
- [ ] `CLIENT_ARCHITECTURE.md` (technical deep-dive)

### Modified Files (Estimated 8 files)
- [ ] `src/state/app_state.rs`
- [ ] `src/protocol/mod.rs`
- [ ] `src/llm/actions/common.rs`
- [ ] `src/llm/actions/executor.rs`
- [ ] `src/cli/commands.rs`
- [ ] `src/cli/rolling_tui.rs`
- [ ] `src/events.rs`
- [ ] `CLAUDE.md`

### Total Estimated Impact
- **New Files**: ~28
- **Modified Files**: ~8
- **New Lines of Code**: ~4500
- **Modified Lines of Code**: ~500
- **Total Lines**: ~5000

---

## Conclusion

This plan provides a comprehensive roadmap for adding LLM-controlled client capability to NetGet by mirroring the existing server architecture. The phased approach allows for incremental validation while maintaining the project's core principles of decentralization, flexibility, and LLM control.

The key innovation is treating clients as peer components to servers, with their own trait system, state management, and action system, rather than trying to force-fit client behavior into the server model.

Next steps:
1. Review this plan with project stakeholders
2. Begin Phase 1 (Core Infrastructure)
3. Validate architecture with TCP client (Phase 3)
4. Iterate and expand based on learnings
