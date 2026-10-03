# SVN Protocol Client Implementation Plan

## ⚠️ Status: NOT IMPLEMENTED (Planning Document)

This document outlines the implementation plan for an SVN protocol client that complements the existing SVN server implementation. This is a **planning and feasibility document** - the client is not yet implemented.

## Overview

**Goal**: Implement a pure Rust SVN protocol client using custom S-expression parsing to communicate with SVN servers over TCP (port 3690).

**Complexity**: 🔴 **VERY HARD** (custom protocol parsing, no library support, binary data challenges)

**Estimated Implementation Time**: 21-31 days (3-4 weeks full-time)

**Alternative Approach**: Command wrapper client (3-5 days) - see "Command Wrapper Alternative" section below

## Why This Is Challenging

### 1. No Rust Library Support

Unlike HTTP, DNS, or even Redis, there is **NO pure Rust SVN client library**:

- **subversion crate (v0.0.8)**: FFI bindings to C library (libsvn)
  - Requires system installation of Apache Subversion
  - Extremely immature (0.0.8 version)
  - Complex C API surface (libsvn_client, libsvn_ra, libsvn_wc, etc.)
  - Limited documentation
  - Platform-specific dependencies

- **No alternatives**: No other Rust crates exist for SVN protocol

**Consequence**: Must implement entire protocol from scratch, including:
- S-expression parser/serializer
- Protocol handshake and authentication
- Binary data handling (svndiff format)
- Command construction and response parsing
- Error handling and recovery

### 2. Complex Custom Protocol

SVN protocol (`svn://`) is significantly more complex than protocols like Redis or DNS:

#### S-Expression Syntax
```
( command-name arg1 arg2 ( nested list ) "quoted string" )
```

**Challenges**:
- Recursive parsing required (nested lists)
- Multiple data types (strings, numbers, lists, booleans)
- Whitespace handling
- Quote escaping
- No standardized S-expression parser in Rust ecosystem

#### Two-Phase Protocol

**Phase 1: Greeting Exchange**
```
Server → Client: ( success ( 2 2 ( ANONYMOUS ) ( edit-pipeline svndiff1 ... ) ) )
                           ↑   ↑   ↑                 ↑
                           min max auth              capabilities
                           ver ver mechanisms

Client → Server: ( 2 ( edit-pipeline svndiff1 ) 127.0.0.1:0 ( ANONYMOUS ( ) ) )
```

**Phase 2: Command Exchange**
```
Client → Server: ( get-dir ( "/trunk" 42 false true ) )
                            ↑         ↑  ↑     ↑
                            path      rev props contents

Server → Client: ( success ( ( entries ( ( name "src" kind dir ... ) ... ) ) ) )
```

**Challenges**:
- State machine management (Connecting → Greeting → Authenticated → Command)
- Protocol version negotiation
- Capability negotiation (edit-pipeline, svndiff1, absent-entries, etc.)
- Authentication mechanism selection (ANONYMOUS, CRAM-MD5, etc.)

### 3. Binary Data Handling

Unlike the simplified server implementation, a real client must handle:

**svndiff Format** (delta compression):
- Binary deltas for efficient file transfer
- Window-based compression
- Source/target length encoding
- Instruction streams (copy/insert operations)

**Text Deltas**:
- Line-based diffs
- Property changes
- MIME type handling

**Challenges**:
- Cannot represent binary data in LLM actions (per CLAUDE.md guidelines)
- Must abstract binary operations into structured LLM actions
- Complex parsing logic for delta decoding
- Memory management for large files

### 4. Authentication Complexity

SVN supports multiple authentication mechanisms:

- **ANONYMOUS**: No credentials (simplest)
- **CRAM-MD5**: Challenge-response
- **EXTERNAL**: OS-level authentication
- **GSSAPI**: Kerberos
- **SASL**: Various SASL mechanisms

**Client must**:
- Parse server's offered mechanisms
- Choose appropriate mechanism
- Perform authentication handshake
- Handle authentication failures

**LLM integration challenge**: How does LLM provide credentials without exposing secrets in action logs?

### 5. Command Set Complexity

Unlike Redis (simple command strings), SVN has complex structured commands:

**Directory Listing**:
```
( get-dir ( "/path" 42 true true ) )
          ↑        ↑  ↑    ↑
          path     rev want_props want_contents
```

**File Retrieval**:
```
( get-file ( "/path/file.txt" 42 ( ) ) )
```

**Log Retrieval**:
```
( log ( ( "/trunk" ) 1 42 true true false ( ) ) )
       ↑            ↑ ↑  ↑    ↑    ↑     ↑
       paths        s e  changed_paths strict_node_history limit
```

**Update Operation** (extremely complex):
- Editor commands for working copy management
- Delta transmission
- Conflict resolution
- Property merging

**Challenges**:
- LLM must construct correct command structures
- Type safety (strings vs numbers vs booleans vs lists)
- Argument ordering matters
- Missing optional arguments must be represented as `( )`

### 6. LLM Action Design Challenges

Per CLAUDE.md guidelines, **NEVER use bytes or base64 in actions**. This creates challenges:

**Problem**: How to represent binary file contents?

**Bad (violates guidelines)**:
```json
{
  "type": "get_svn_file",
  "path": "/trunk/image.png",
  "content_base64": "iVBORw0KGgoAAAANSUhEUgAA..."  // ❌ LLM can't construct this
}
```

**Good (structured data)**:
```json
{
  "type": "get_svn_file",
  "path": "/trunk/README.txt",
  "revision": 42,
  "want_contents": true
}
```

But then **where does the binary data go**? Client must:
- Store binary data internally
- Provide structured metadata to LLM
- Allow LLM to reference files by path/revision
- Handle file writes without LLM seeing bytes

**Complexity**: Two-tier architecture (binary layer + LLM layer)

## Implementation Plan (7 Phases)

### Phase 1: Protocol Foundation (3-5 days)

**Goal**: Build S-expression parser and serializer

**Files to Create**:
- `src/client/svn/protocol.rs` - S-expression parsing
- Unit tests for parser

**S-Expression Parser Requirements**:
```rust
enum SExpression {
    Atom(String),           // word or "quoted string"
    Number(i64),
    List(Vec<SExpression>),
}

fn parse_sexp(input: &str) -> Result<SExpression>;
fn serialize_sexp(expr: &SExpression) -> String;
```

**Test Cases**:
- Simple atoms: `word`, `"quoted string"`
- Numbers: `42`, `-5`, `0`
- Flat lists: `( a b c )`
- Nested lists: `( a ( b c ) d )`
- Mixed types: `( get-dir ( "/trunk" 42 true false ) )`
- Edge cases: Empty lists `( )`, escaped quotes `"say \"hi\""`

**Hardships**:
- No standard S-expression library in Rust
- Recursive parsing complexity
- Performance (must parse many responses quickly)
- Error recovery (malformed S-expressions)

**Can Reuse**: Server's parser logic from `src/server/svn/mod.rs:151-200`

### Phase 2: Core Client Structure (5-7 days)

**Goal**: TCP connection, protocol handshake, LLM integration

**Files to Create**:
- `src/client/svn/mod.rs` - Main client implementation
- `src/client/svn/actions.rs` - Client trait implementation

**Connection State Machine**:
```rust
enum SvnClientState {
    Connecting,           // TCP in progress
    AwaitingGreeting,     // Waiting for server greeting
    Authenticating,       // Auth handshake
    Authenticated,        // Ready for commands
    CommandInProgress,    // Waiting for response
    Disconnected,
}
```

**Handshake Sequence**:
1. Connect TCP to `remote_addr:3690`
2. Read server greeting: `( success ( min-ver max-ver ( mechanisms ) ( caps ) ) )`
3. Parse version, capabilities, auth mechanisms
4. Send client response: `( 2 ( caps ) 127.0.0.1:0 ( ANONYMOUS ( ) ) )`
5. Transition to Authenticated state
6. Call LLM with `svn_connected` event

**LLM Integration**:
```rust
pub async fn connect_with_llm_actions(
    remote_addr: SocketAddr,
    llm_client: LlmClient,
    app_state: AppState,
    status_tx: UnboundedSender<String>,
    client_id: ClientId,
) -> Result<SocketAddr> {
    // 1. TCP connect
    let stream = TcpStream::connect(remote_addr).await?;

    // 2. Protocol handshake
    let greeting = Self::read_greeting(&stream).await?;
    Self::send_auth_response(&stream, greeting).await?;

    // 3. Update client status
    app_state.update_client_status(client_id, ClientStatus::Connected)?;

    // 4. Call LLM with connected event
    let event = Event::new(&SVN_CLIENT_CONNECTED_EVENT, json!({
        "server_version": greeting.version,
        "capabilities": greeting.capabilities,
    }));

    call_llm_for_client(&llm_client, &app_state, client_id, Some(&event), &status_tx).await?;

    // 5. Spawn read loop
    let (read_half, write_half) = tokio::io::split(stream);
    let write_half = Arc::new(Mutex::new(write_half));

    tokio::spawn(Self::read_loop(client_id, read_half, write_half.clone(), llm_client, app_state, status_tx));

    Ok(remote_addr)
}
```

**Read Loop**:
```rust
async fn read_loop(...) {
    loop {
        // Read S-expression response
        let response = read_sexp_response(&mut read_half).await?;

        // Parse response type
        match response {
            SExpression::List(items) => {
                match items[0].as_str()? {
                    "success" => {
                        // Extract data
                        let data = parse_response_data(&items[1])?;

                        // Call LLM with response event
                        let event = Event::new(&SVN_CLIENT_RESPONSE_RECEIVED_EVENT, json!({
                            "status": "success",
                            "data": data,
                        }));

                        let result = call_llm_for_client(..., Some(&event), ...).await?;

                        // Execute actions
                        for action in result.actions {
                            self.execute_action(action)?;
                        }
                    }
                    "failure" => {
                        // Handle error
                    }
                    _ => warn!("Unknown response type"),
                }
            }
        }
    }
}
```

**Hardships**:
- Tokio async complexity (split streams, Arc<Mutex<WriteHalf>>)
- Error propagation across async boundaries
- Graceful shutdown (read loop must detect disconnection)
- Connection timeout handling
- Buffering strategy (SVN responses can be large)

### Phase 3: Event and Action Definitions (3-4 days)

**Goal**: Define EventType constants and action definitions for LLM

**Event Types**:

```rust
use std::sync::LazyLock;

pub static SVN_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new("svn_connected", "SVN client successfully connected and authenticated to server")
        .with_parameters(vec![
            EventParameter::new("server_version", "Server protocol version (typically '2')"),
            EventParameter::new("capabilities", "Array of server capabilities (e.g., ['edit-pipeline', 'svndiff1'])"),
            EventParameter::new("auth_mechanisms", "Available authentication mechanisms (e.g., ['ANONYMOUS', 'CRAM-MD5'])"),
            EventParameter::new("remote_addr", "Server address and port"),
        ])
});

pub static SVN_CLIENT_RESPONSE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new("svn_response_received", "SVN server sent a response to a command")
        .with_parameters(vec![
            EventParameter::new("status", "Response status: 'success' or 'failure'"),
            EventParameter::new("command", "Command that generated this response"),
            EventParameter::new("data", "Response data (structure varies by command)"),
            EventParameter::new("error_code", "SVN error code (if failure)"),
            EventParameter::new("error_message", "Error message (if failure)"),
        ])
});
```

**Sync Actions** (Response to server events):

```rust
impl Client for SvnClientProtocol {
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition::new("send_svn_command")
                .with_description("Send an SVN protocol command to the server")
                .with_parameter("command", "Command name (e.g., 'get-latest-rev', 'get-dir', 'get-file')")
                .with_parameter("args", "Array of command arguments (optional)"),

            ActionDefinition::new("get_latest_revision")
                .with_description("Get the latest revision number from the repository"),

            ActionDefinition::new("list_directory")
                .with_description("List contents of a directory")
                .with_parameter("path", "Directory path (e.g., '/trunk', '/branches')")
                .with_parameter("revision", "Revision number (use -1 for HEAD)")
                .with_parameter("want_properties", "Include properties (boolean)"),

            ActionDefinition::new("get_file")
                .with_description("Retrieve file contents and metadata")
                .with_parameter("path", "File path")
                .with_parameter("revision", "Revision number"),

            ActionDefinition::new("get_log")
                .with_description("Get commit log for path(s)")
                .with_parameter("paths", "Array of paths")
                .with_parameter("start_revision", "Start revision")
                .with_parameter("end_revision", "End revision")
                .with_parameter("limit", "Maximum number of log entries"),

            ActionDefinition::new("disconnect")
                .with_description("Close the SVN connection"),

            ActionDefinition::new("wait_for_more")
                .with_description("Wait for more data from server"),
        ]
    }
}
```

**Async Actions** (User-triggered):

```rust
fn get_async_actions(&self, state: &AppState) -> Vec<ActionDefinition> {
    vec![
        ActionDefinition::new("modify_client_instruction")
            .with_description("Update the SVN client's instruction")
            .with_parameter("instruction", "New instruction for the client"),

        ActionDefinition::new("reconnect_svn_client")
            .with_description("Reconnect to the SVN server"),
    ]
}
```

**Action Execution**:

```rust
fn execute_action(&self, action: Value, write_half: Arc<Mutex<WriteHalf>>) -> Result<ClientActionResult> {
    let action_type = action["type"].as_str()?;

    match action_type {
        "send_svn_command" => {
            let command = action["command"].as_str()?;
            let args = action.get("args")
                .and_then(|v| v.as_array())
                .unwrap_or(&vec![]);

            // Build S-expression
            let sexp = build_command_sexp(command, args)?;

            // Serialize to string
            let command_str = serialize_sexp(&sexp)?;

            Ok(ClientActionResult::SendData(command_str.into_bytes()))
        }

        "get_latest_revision" => {
            let sexp = SExpression::List(vec![
                SExpression::Atom("get-latest-rev".to_string()),
            ]);
            let command_str = serialize_sexp(&sexp)?;
            Ok(ClientActionResult::SendData(command_str.into_bytes()))
        }

        "list_directory" => {
            let path = action["path"].as_str()?;
            let revision = action["revision"].as_i64().unwrap_or(-1);
            let want_props = action["want_properties"].as_bool().unwrap_or(true);

            let sexp = SExpression::List(vec![
                SExpression::Atom("get-dir".to_string()),
                SExpression::List(vec![
                    SExpression::Atom(format!("\"{}\"", path)),
                    SExpression::Number(revision),
                    SExpression::Atom(want_props.to_string()),
                    SExpression::Atom("true".to_string()),  // want_contents
                ]),
            ]);

            let command_str = serialize_sexp(&sexp)?;
            Ok(ClientActionResult::SendData(command_str.into_bytes()))
        }

        "disconnect" => Ok(ClientActionResult::Disconnect),

        "wait_for_more" => Ok(ClientActionResult::WaitForMore),

        _ => bail!("Unknown action type: {}", action_type),
    }
}
```

**Hardships**:
- Action design requires deep protocol knowledge
- Balancing simplicity (for LLM) vs expressiveness (for users)
- Type safety in JSON action parameters
- Error messages must be actionable for LLM
- Documentation must explain SVN concepts to LLM

### Phase 4: Command Support (4-6 days)

**Goal**: Implement high-level helpers for common SVN commands

**Commands to Support**:

1. **Repository Info**
   - `get-latest-rev` → Latest revision number
   - `get-uuid` → Repository UUID
   - `check-path` → Verify path exists

2. **Directory Operations**
   - `get-dir` → List directory contents
   - Response parsing: Extract entries with (name, kind, size, revision)

3. **File Operations**
   - `get-file` → Get file contents and properties
   - Response parsing: Extract checksum, size, content

4. **Log Operations**
   - `log` → Get commit history
   - Response parsing: Extract (revision, author, date, message, changed_paths)

5. **Diff Operations**
   - `diff` → Get differences (simplified, no binary deltas)

6. **Property Operations**
   - `get-file-revs` → Get revision history for file
   - `rev-proplist` → Get revision properties

**Response Parsing Complexity**:

Each command returns different S-expression structure:

**get-latest-rev**:
```
( success ( 42 ) )
```

**get-dir**:
```
( success (
    ( entries (
        ( "trunk" ( kind dir size 0 has-props false created-rev 1 ... ) )
        ( "branches" ( kind dir size 0 has-props false created-rev 1 ... ) )
    ) )
) )
```

**get-file**:
```
( success (
    ( checksum "md5:..." )
    ( contents ( size 1234 ) )
    ( props ( ) )
) )
```

**log**:
```
( success (
    ( log-entry (
        ( revision 42 )
        ( author "dev@example.com" )
        ( date "2024-01-01T12:00:00Z" )
        ( message "Fix bug" )
        ( changed-paths (
            ( "/trunk/file.txt" ( action M ... ) )
        ) )
    ) )
) )
```

**Parsing Strategy**:
```rust
fn parse_dir_response(sexp: &SExpression) -> Result<Vec<DirEntry>> {
    // Navigate nested structure
    let success = sexp.as_list()?;
    let data = success[1].as_list()?;
    let entries = find_key(data, "entries")?;

    let mut result = vec![];
    for entry in entries.as_list()? {
        let name = entry[0].as_string()?;
        let props = entry[1].as_list()?;

        result.push(DirEntry {
            name,
            kind: find_key(props, "kind")?.as_string()?,
            size: find_key(props, "size")?.as_number()?,
            revision: find_key(props, "created-rev")?.as_number()?,
        });
    }

    Ok(result)
}
```

**Hardships**:
- Each command has unique response structure
- Nested S-expressions require recursive parsing
- Error handling (missing keys, wrong types)
- Version compatibility (response format may change)
- Large responses (directory with thousands of files)

### Phase 5: Testing (3-4 days)

**Goal**: E2E tests with NetGet SVN server (< 10 LLM calls)

**Test File**: `tests/client/svn/e2e_test.rs`

**Test Strategy**:

1. **Connection Test** (1 LLM call)
   - Start SVN server (mocked or scripted)
   - Connect SVN client
   - Verify handshake and authentication

2. **Get Latest Revision Test** (1 LLM call)
   - Server instruction: "Latest revision is 42"
   - Client instruction: "Get the latest revision"
   - Verify client receives revision 42

3. **List Directory Test** (1 LLM call)
   - Server instruction: "Repository has trunk/, branches/, tags/"
   - Client instruction: "List root directory"
   - Verify client receives 3 entries

4. **Get File Test** (1 LLM call)
   - Server instruction: "File /trunk/README.txt contains 'Hello World'"
   - Client instruction: "Get /trunk/README.txt at revision 1"
   - Verify client receives file contents

5. **Error Handling Test** (1 LLM call)
   - Server instruction: "Return 'Path not found' for any invalid path"
   - Client instruction: "Try to get /nonexistent"
   - Verify client receives error response

**Example Test**:

```rust
#[cfg(all(test, feature = "svn"))]
mod svn_client_tests {
    use super::*;

    #[tokio::test]
    async fn test_svn_client_get_latest_revision() -> Result<()> {
        // Start SVN server with mocked responses
        let server = NetGetConfig::new("listen on {AVAILABLE_PORT} via svn")
            .with_instruction("Repository is at revision 42. Respond to get-latest-rev with revision 42.")
            .start_server()
            .await?;

        // Start SVN client
        let client = NetGetConfig::new(&format!(
            "connect to 127.0.0.1:{} via svn",
            server.port()
        ))
        .with_instruction("Get the latest revision number from the repository")
        .start_client()
        .await?;

        // Wait for LLM processing
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Verify client connected and got response
        let client_state = client.get_client_state()?;
        assert_eq!(client_state.status, ClientStatus::Connected);

        // Verify command was sent and response received
        // (depends on test infrastructure for event inspection)

        Ok(())
    }
}
```

**LLM Call Budget**: ~5 tests × 1 call each = 5 LLM calls total

**Hardships**:
- Test infrastructure may not support client event inspection yet
- Mock server responses must match protocol exactly
- Timing issues (LLM may be slow, tests may timeout)
- Asserting on internal state vs external behavior
- Flakiness from Ollama variability

### Phase 6: Integration (2-3 days)

**Goal**: Wire up client into NetGet infrastructure

**Registry Registration** (`src/protocol/client_registry.rs`):
```rust
#[cfg(feature = "svn")]
registry.register(Arc::new(crate::client::svn::SvnClientProtocol));
```

**Module Export** (`src/client/mod.rs`):
```rust
#[cfg(feature = "svn")]
pub mod svn;
```

**Startup Handler** (`src/cli/client_startup.rs`):
```rust
#[cfg(feature = "svn")]
"svn" => {
    crate::client::svn::SvnClient::connect_with_llm_actions(
        remote_addr,
        llm_client,
        app_state,
        status_tx,
        client_id,
    ).await?
}
```

**Feature Flag** (`Cargo.toml`):
```toml
[features]
svn = []  # No external dependencies!
all-protocols = [..., "svn"]
```

**Client State** (`src/state/client.rs`):
- May need SVN-specific connection info (repository_url, current_revision)

**Hardships**:
- Merge conflicts if other protocols added concurrently
- Feature flag testing (must compile with/without svn feature)
- Documentation updates (README, protocol lists)
- TUI display (show SVN client status)

### Phase 7: Documentation (1-2 days)

**Goal**: Comprehensive documentation for maintainers

**This File** (`src/client/svn/CLAUDE.md`):
- Implementation details (completed phases)
- Protocol references
- Known issues and limitations

**Test Documentation** (`tests/client/svn/CLAUDE.md`):
- Test strategy and rationale
- LLM call budget breakdown
- How to run tests
- Known flaky tests

**Code Comments**:
- Inline documentation for complex parsing logic
- Protocol references in comments
- Example S-expressions in function headers

## Command Wrapper Alternative (RECOMMENDED)

Given the immense complexity of a protocol client, a **command wrapper client** is far more practical:

### Implementation Strategy

Instead of parsing SVN protocol, **execute the `svn` CLI tool** and let the LLM construct commands:

```rust
// src/client/svn/wrapper.rs
pub async fn execute_svn_command(args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("svn")
        .args(args)
        .output()
        .await?;

    Ok(String::from_utf8(output.stdout)?)
}
```

**LLM Actions**:

```json
{
  "type": "svn_checkout",
  "url": "https://svn.example.com/repo",
  "path": "/tmp/myrepo"
}
```

Executes: `svn checkout https://svn.example.com/repo /tmp/myrepo`

```json
{
  "type": "svn_update",
  "path": "/tmp/myrepo"
}
```

Executes: `svn update /tmp/myrepo`

```json
{
  "type": "svn_log",
  "path": "/tmp/myrepo",
  "limit": 10
}
```

Executes: `svn log /tmp/myrepo -l 10`

### Advantages

✅ **Fast implementation**: 3-5 days vs 21-31 days

✅ **Reliable**: Uses battle-tested Apache Subversion CLI

✅ **No protocol complexity**: No S-expression parsing, no binary data handling

✅ **Full feature support**: All SVN commands available (checkout, commit, merge, etc.)

✅ **No dependencies**: No Rust crates, just system `svn` binary

✅ **Easy testing**: Run actual SVN commands, verify output

### Disadvantages

❌ **Requires system installation**: `svn` binary must be installed

❌ **Not a "pure" protocol client**: Doesn't work with NetGet SVN server

❌ **Process overhead**: Spawning subprocess for each command

❌ **Platform-specific**: Different SVN versions, different output formats

### When to Use Each Approach

| Approach | Use When |
|----------|----------|
| **Protocol Client** | - Testing NetGet SVN server<br>- Learning SVN protocol<br>- No system `svn` available<br>- Need programmatic control |
| **Command Wrapper** | - Real repository access<br>- Production use<br>- Full SVN feature set needed<br>- Fast implementation required |

## Comparison with Other Clients

### TCP Client (SIMPLE ✅)

- **Library**: None needed (raw sockets)
- **Protocol**: Bytes in, bytes out
- **LLM Actions**: Hex-encoded data
- **Implementation Time**: 3-5 days
- **Complexity**: Low

### HTTP Client (MEDIUM ⚠️)

- **Library**: `reqwest` (mature, stable)
- **Protocol**: Well-standardized (RFC 7230-7235)
- **LLM Actions**: Structured (method, path, headers, body)
- **Implementation Time**: 5-7 days
- **Complexity**: Medium (TLS, redirects, cookies)

### Redis Client (MEDIUM ⚠️)

- **Library**: `redis` crate available (but NetGet uses custom)
- **Protocol**: Simple text-based (RESP)
- **LLM Actions**: Command strings ("GET key", "SET key value")
- **Implementation Time**: 5-7 days
- **Complexity**: Medium (pipelining, pub/sub)

### SVN Client (VERY HARD 🔴)

- **Library**: None (or immature FFI bindings)
- **Protocol**: Complex S-expressions + binary data
- **LLM Actions**: Structured (but must abstract binary operations)
- **Implementation Time**: 21-31 days
- **Complexity**: Very High (parsing, binary data, auth, state management)

**SVN is 3-6× more complex than HTTP or Redis client implementation.**

## Known Issues and Limitations (Anticipated)

### 1. No Binary Data Support (Initially)

- First implementation will handle text commands only
- Binary file transfers (svndiff) deferred to Phase 2
- Limits usefulness (can't checkout real repositories with binary files)

### 2. ANONYMOUS Authentication Only

- No CRAM-MD5, GSSAPI, or other mechanisms
- Can only connect to servers allowing anonymous access
- Credential management is complex (how does LLM provide passwords?)

### 3. No Working Copy Management

- Client doesn't maintain `.svn` working copy metadata
- Can't perform local operations (status, diff without network)
- Every operation requires server round-trip

### 4. No Update/Commit Support (Initially)

- Read-only operations first (get-dir, get-file, log)
- Update/commit require editor commands (extremely complex)
- Deferred to later phases

### 5. Protocol Version 2 Only

- No support for version 1 (older servers)
- No support for experimental newer versions
- May not work with all SVN servers

### 6. Large Response Handling

- Directory with 10,000+ files may overwhelm parser
- Log with thousands of commits may timeout
- Need pagination/streaming support

### 7. Performance

- Each LLM call takes 2-5 seconds
- SVN operations require multiple commands (slow without scripting)
- Not suitable for large checkouts (use command wrapper instead)

## References and Resources

### SVN Protocol Documentation

- **Official Protocol Spec**: https://svn.apache.org/repos/asf/subversion/trunk/subversion/libsvn_ra_svn/protocol
  - Authoritative protocol definition
  - S-expression format specification
  - Command reference
  - Error codes

- **Protocol Analysis**: https://cwiki.apache.org/confluence/display/SVN/ProtocolAnalysis
  - High-level overview
  - Sequence diagrams
  - Common patterns

- **SVN Book**: https://svnbook.red-bean.com/
  - User-facing documentation
  - Conceptual explanations
  - Not protocol-specific but helpful for understanding operations

### Rust Libraries (Limited)

- **subversion crate**: https://crates.io/crates/subversion (v0.0.8)
  - FFI bindings to C library
  - Very immature
  - Requires system libsvn installation

- **subversion-rs**: https://github.com/jelmer/subversion-rs
  - Source repository for subversion crate
  - Limited documentation
  - Active development (but slow)

### NetGet Server Implementation

- **Server CLAUDE.md**: `src/server/svn/CLAUDE.md`
  - Explains S-expression parsing strategy
  - Shows response formatting
  - Documents supported commands

- **Server Implementation**: `src/server/svn/mod.rs`
  - Parser logic (lines 151-200) can be reused
  - Response builders show correct S-expression format

- **Server Actions**: `src/server/svn/actions.rs`
  - Action definitions mirror what client needs
  - Event parameters show what data is available

### S-Expression Parsing

- **Common Lisp**: https://www.cs.cmu.edu/Groups/AI/html/cltl/clm/node3.html
  - S-expression origins (Lisp syntax)
  - Parsing techniques

- **serde-lexpr**: https://crates.io/crates/serde-lexpr
  - Rust crate for S-expression serialization
  - May be useful for parsing (but SVN S-expressions are non-standard)

### Other Clients (for Pattern Reference)

- **TCP Client**: `src/client/tcp/` - Simplest client pattern
- **HTTP Client**: `src/client/http/` - Library-based pattern
- **Redis Client**: `src/client/redis/` - Text protocol pattern

### Testing References

- **CLIENT_PROTOCOL_FEASIBILITY.md**: Marks SVN as unfeasible (rationale for command wrapper)
- **Test Infrastructure**: `tests/server/helpers.rs` - Testing utilities

## Decision Matrix: Should You Implement This?

### ✅ Implement Protocol Client If:

- You want to **test the NetGet SVN server** with a real client
- You're **learning the SVN protocol** in depth
- You need **programmatic control** over protocol internals
- You have **3-4 weeks** to dedicate to implementation
- You enjoy **complex parsing challenges**
- You don't have access to system `svn` binary

### ❌ Use Command Wrapper If:

- You need **real repository access** (checkout, commit, merge)
- You want **fast implementation** (3-5 days)
- You need **production reliability**
- You want **full SVN feature support**
- You have `svn` binary installed on your system

### 🤔 Evaluate Further If:

- Uncertain about use case (test vs production)
- Could start with wrapper, add protocol client later
- Need to demo quickly (wrapper), then optimize (protocol)

## Conclusion

Implementing an SVN protocol client is a **significant undertaking** requiring:

- **Deep protocol knowledge** (S-expressions, binary data, auth)
- **Robust parsing** (recursive, error-tolerant)
- **Complex LLM integration** (abstracting binary operations)
- **Extensive testing** (protocol compliance)
- **3-4 weeks full-time effort**

**Alternative**: Command wrapper client provides 80% of the value in 20% of the time.

**Recommendation**: Start with command wrapper for practical use, implement protocol client only if:
1. Testing NetGet SVN server is critical
2. Protocol internals understanding is valuable
3. Time investment is justified

This planning document should be reviewed before implementation begins. If you proceed with protocol client, update this document with actual implementation details, challenges encountered, and lessons learned.

## Next Steps (If Proceeding)

1. **Review this plan** with stakeholders
2. **Decide**: Protocol client or command wrapper?
3. **If protocol client**:
   - Start with Phase 1 (S-expression parser)
   - Create unit tests for parser
   - Test parser against server greeting format
   - Evaluate complexity after Phase 1
   - Decide whether to continue or pivot to wrapper
4. **If command wrapper**:
   - See `git` client for similar pattern (if it exists)
   - Implement CLI execution logic
   - Parse stdout/stderr
   - Map LLM actions to `svn` commands
   - Test against real SVN repository

Good luck! 🚀
