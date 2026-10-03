# DC (Direct Connect) Client Implementation

## Overview

DC (Direct Connect) is a peer-to-peer file sharing protocol where a central "hub" coordinates client connections. This client implementation allows NetGet to connect to DC hubs as a client, authenticate using the NMDC protocol, participate in chat, search for files, and interact with other users.

**Status**: Experimental (Application Protocol)
**RFC**: No official RFC (community-maintained specification)
**Specification**: https://nmdc.sourceforge.io/NMDC.html
**Port**: 411 (plain TCP), 412 (with TLS, not implemented)

## Library Choices

- **No DC library** - Manual protocol implementation
    - NMDC is a text-based protocol (simple parsing)
    - Commands prefixed with `$`, terminated with `|` (pipe)
    - Responses constructed as formatted strings
    - No complex binary protocol needed
- **tokio** - Async runtime and I/O
    - `TcpStream` for connection to hub
    - `BufReader` for reading pipe-delimited messages
    - `AsyncWriteExt` for sending commands

**Rationale**: The NMDC protocol is simple enough that no dedicated library is needed. Messages are text terminated with `|`, commands start with `$`. Manual implementation gives LLM full control and avoids library constraints. No existing Rust DC client library was found on crates.io.

## Architecture Decisions

### 1. NMDC Protocol Only

We implement NMDC instead of ADC because:

- NMDC is simpler and more widely supported
- Most DC++ clients support NMDC
- Text-based protocol is easier for LLM to understand and control
- ADC can be added later as an enhancement

### 2. Client-to-Hub Communication

The client connects to a hub and:

- Authenticates using Lock/Key challenge-response
- Registers a nickname with `$ValidateNick`
- Sends client information with `$MyINFO`
- Participates in public chat
- Sends/receives private messages
- Searches for files via hub coordination
- Receives user lists and hub information

### 3. Lock/Key Authentication

NMDC uses a challenge-response authentication:

1. Hub sends `$Lock <lock_string> Pk=<hub_name>|`
2. Client calculates key from lock using NMDC algorithm
3. Client sends `$Key <calculated_key>|`
4. Client sends `$ValidateNick <nickname>|`
5. Hub sends `$Hello <nickname>|` to accept
6. Client is now authenticated and can chat/search

**Key Calculation Algorithm** (implemented in `calculate_dc_key`):
```
For each byte in lock:
    key[i] = lock[i] ^ lock[i-1]

key[0] = lock[0] ^ lock[len-1] ^ lock[len-2] ^ 5

Nibble swap each byte: (byte << 4) | (byte >> 4)
Escape special characters: 0, 5, 36 ($), 96 (`), 124 (|), 126 (~)
```

### 4. State Machine

**States**:
- `AwaitingLock` - Waiting for hub's `$Lock` challenge
- `AwaitingHello` - Sent `$Key` + `$ValidateNick`, waiting for `$Hello` acceptance
- `Authenticated` - Can chat, search, and interact with hub

**Transitions**:
- Connect → `AwaitingLock` (on TCP connection)
- `AwaitingLock` → `AwaitingHello` (received `$Lock`, sent `$Key` + `$ValidateNick`)
- `AwaitingHello` → `Authenticated` (received `$Hello`)

### 5. Message Parsing

DC messages are framed on `|` (pipe), **not** newline — a real hub's `$Lock` greeting carries
no `\n`, so line-based reading would stall until the connection closed. The read loop uses
`read_until(b'|', ..)`, yielding one complete message per read; `BufReader` keeps any bytes
after the delimiter buffered, so partial messages carry across reads:

```rust
// One pipe-terminated message per read
reader.read_until(b'|', &mut buf).await?;
let segment = String::from_utf8_lossy(&buf).trim_end_matches('|').trim();
if segment.is_empty() { continue; }

if segment.starts_with("$Lock ") {
    handle_lock_message(...);
} else if segment.starts_with("$Hello ") {
    handle_hello_message(...);
} else if segment.starts_with("<") {
    handle_chat_message(...);
} // ... etc
```

### 6. LLM Integration

The LLM receives events for each message type and can respond with actions:

**Events** (Hub → Client → LLM):
- `dc_client_connected` - Received `$Lock` challenge
- `dc_client_authenticated` - Received `$Hello`, fully authenticated
- `dc_client_message_received` - Chat message (public or private)
- `dc_client_search_result` - Search result from hub
- `dc_client_userlist_received` - List of connected users
- `dc_client_hubinfo_received` - Hub name, topic
- `dc_client_kicked` - Kicked from hub
- `dc_client_redirect` - Redirected to another hub
- `dc_client_disconnected` - Disconnected from hub (with reconnection info)

**Actions** (LLM → Client → Hub):
- `send_dc_chat` - Send public chat message
- `send_dc_private_message` - Send private message to user
- `send_dc_search` - Search for files
- `send_dc_myinfo` - Send/update client information
- `send_dc_get_nicklist` - Request user list
- `send_dc_filelist` - Configure file list for hub requests
- `send_dc_raw_command` - Send raw NMDC command
- `disconnect` - Disconnect from hub

### 7. Dual Logging

- **DEBUG**: Connection events, authentication flow ("DC client 1 authenticated as 'alice'")
- **INFO**: Important messages, state changes
- **TRACE**: Full message text for debugging
- All logs go to netget.log and TUI Status panel

### 8. Startup Parameters

The client requires configuration via startup parameters:

- `nickname` (required) - Nickname to use on the hub
- `description` (optional) - Client description (default: "NetGet DC Client")
- `email` (optional) - Email address
- `share_size` (optional) - Total bytes shared (fake for testing, default: 0)
- `use_tls` (optional) - Use TLS encryption (default: false)
- `auto_reconnect` (optional) - Automatically reconnect if disconnected (default: false)
- `max_reconnect_attempts` (optional) - Maximum reconnection attempts, 0 = unlimited (default: 5)
- `initial_reconnect_delay_secs` (optional) - Initial delay before reconnecting with exponential backoff (default: 2)

Example:
```json
{
  "nickname": "alice",
  "description": "NetGet DC Test Client",
  "email": "alice@example.com",
  "share_size": 1073741824,
  "use_tls": true,
  "auto_reconnect": true,
  "max_reconnect_attempts": 10,
  "initial_reconnect_delay_secs": 2
}
```

### 9. Dashboard Injection (command channel)

The loop registers a command channel (`command_support::register_command_channel`) before the
read loop starts - i.e. before the `$Lock`-triggered connected-event LLM call, which a manual
rule can park. `read_until` is not cancellation-safe, so commands are drained by a separate
`command_loop` task (registered with `register_client_task`) sharing the `Arc<Mutex<DcWriteHalf>>`.
Every `send_dc_*` verb yields `ClientActionResult::Custom`, so the generic arm cannot run them;
`command_loop` routes the result through `apply_dc_action` - the same function the LLM path
uses - records an `injected_action` access-log entry and replies with `Sent{n}` /
`Executed` (`send_dc_filelist` writes nothing) / `Disconnected` (writes `$Quit|` and half-closes).
The read loop removes the client handle on EOF and on read error. Test:
`tests/client/dc/command_channel_test.rs` (zero LLM calls).

## LLM Integration

### Event Types

#### `dc_client_connected`

Triggered when client connects to hub and receives Lock challenge.

Event parameters:
- `lock` (string) - Lock challenge from hub
- `pk` (string, optional) - Hub PK name

#### `dc_client_authenticated`

Triggered when hub accepts client with `$Hello`.

Event parameters:
- `nickname` (string) - Accepted nickname

#### `dc_client_message_received`

Triggered when chat message received.

Event parameters:
- `source` (string) - Message source (nickname or "Hub")
- `message` (string) - Message text
- `is_private` (boolean) - True if private message
- `target` (string, optional) - Target nickname (for private messages)

#### `dc_client_search_result`

Triggered when search result received.

Event parameters:
- `source` (string) - User who has the file
- `filename` (string) - File name
- `size` (number) - File size in bytes
- `free_slots` (number) - Available download slots
- `total_slots` (number) - Total download slots
- `hub_name` (string, optional) - Hub name

#### `dc_client_userlist_received`

Triggered when user list received.

Event parameters:
- `users` (array) - Array of nicknames

#### `dc_client_hubinfo_received`

Triggered when hub information received.

Event parameters:
- `hub_name` (string, optional) - Hub name
- `hub_topic` (string, optional) - Hub topic/description

#### `dc_client_kicked`

Triggered when client is kicked from hub.

Event parameters:
- `nickname` (string) - Nickname that was kicked

#### `dc_client_redirect`

Triggered when client is redirected to another hub.

Event parameters:
- `address` (string) - New hub address to connect to

#### `dc_client_disconnected`

Triggered when client disconnects from hub.

Event parameters:
- `reason` (string) - Disconnection reason
- `will_reconnect` (boolean) - Whether auto-reconnect will attempt to reconnect
- `reconnect_attempt` (number, optional) - Current reconnection attempt number (0 if first disconnect)

### Available Actions

See `actions.rs` for complete action list with examples.

## Connection Management

### Connection Lifecycle

1. **Connect**: TCP connection to hub
2. **Authenticate**: Receive Lock → Send Key + ValidateNick → Receive Hello
3. **Active**: Chat, search, receive updates
4. **Disconnect**: Send `$Quit|` and close connection

### State Management

- `DcClientState`: Per-client state (authentication state, nickname, memory)
- Memory: LLM conversation memory persisted across messages
- No persistent storage (LLM handles all chat/search logic)

## Known Limitations

### 1. No Peer-to-Peer File Transfers

**Limitation**: Client doesn't implement P2P connections for actual file transfers

**Reason**: File transfer requires:
- Listening on local port (for passive mode)
- `$ConnectToMe` / `$RevConnectToMe` handling
- Binary file transfer protocol
- Upload queue management

**Workaround**: Client can send `$ConnectToMe` commands but won't handle incoming P2P connections. Primary use case is hub interaction (chat, search coordination).

### 2. No ADC Protocol Support

**Limitation**: NMDC only, no ADC (Advanced Direct Connect) support

**Reason**: ADC is a different protocol with binary encoding, negotiation, features, etc.

**Future Enhancement**: Add ADC support as separate client implementation

### 3. File List Generation

**Feature**: ✅ **IMPLEMENTED** - Basic XML file list generation capability

**Usage**: LLM can configure file list via `send_dc_filelist` action

**Implementation**:
- LLM configures file list with array of file objects (name, size, optional TTH hash)
- Client stores file list and generates NMDC-compliant XML format
- Detects file list requests from hub (`$GetListLen`, `$UGetBlock`)
- Generates proper XML with escaping and TTH support

**Example**:
```json
{
  "type": "send_dc_filelist",
  "files": [
    {
      "name": "document.txt",
      "size": 2048,
      "tth": "ABCD1234567890"
    }
  ]
}
```

**Limitations**:
- No actual file sharing or P2P transfer (hub interaction only)
- No ZLIB compression (file list is uncompressed XML)
- LLM must use `send_dc_raw_command` to manually send file list if hub requests it
- File list is fake/for testing - no real files are served

### 4. Message Parsing

**Feature**: ✅ **IMPROVED** - Robust message parsing with better error handling

**Implementation**:
- Proper NMDC private message parsing (`$To: <target> From: <source> $<<source>> <message>`)
- Unicode-aware chat message parsing using char indices
- Graceful error handling for malformed messages
- Support for common NMDC commands (Lock, Hello, chat, private messages, search results, user lists, hub info, kick, redirect)

**Improvements**:
- Private messages now correctly extract target, source, and message fields
- Chat messages use char-based indexing to support Unicode nicknames and messages
- Parse errors are logged but don't crash the client
- Better handling of edge cases (missing delimiters, empty messages, etc.)

**Remaining Limitations**:
- Some less common NMDC commands not yet implemented ($GetINFO, $MyINFO, $ConnectToMe, etc.)
- File list parsing not implemented (see limitation #3)

### 5. TLS Support (DCCS Protocol)

**Feature**: ✅ **IMPLEMENTED** - Optional TLS encryption support

**Usage**: Set `use_tls: true` in startup parameters to enable TLS (port 412)

**Configuration**:
```json
{
  "nickname": "alice",
  "use_tls": true
}
```

**Details**:
- Uses tokio-rustls for TLS handshake
- Supports SNI (Server Name Indication)
- Root certificates from webpki-roots
- Works with both hostname and IP addresses
- Transparent to application layer (same DC protocol)

### 6. Automatic Reconnection

**Feature**: ✅ **IMPLEMENTED** - Optional automatic reconnection with exponential backoff

**Usage**: Set `auto_reconnect: true` in startup parameters to enable

**Configuration**:
```json
{
  "nickname": "alice",
  "auto_reconnect": true,
  "max_reconnect_attempts": 10,
  "initial_reconnect_delay_secs": 2
}
```

**Details**:
- Reconnects automatically if connection drops or initial connection fails
- Exponential backoff (2s, 4s, 8s, 16s, ..., max 60s)
- Configurable max attempts (0 = unlimited)
- Emits `dc_client_disconnected` event before each reconnection
- LLM can observe reconnection progress through events
- Maintains all connection state (nickname, description, TLS settings)

### 7. No Hub State Caching

**Limitation**: Client doesn't cache user lists, hub info, etc.

**Reason**: LLM maintains state through conversation memory

**Workaround**: LLM can request `$GetNickList|` anytime to refresh user list

## Example Prompts

### Basic Connection
```
open_client dc hub.example.com:411 --nickname alice "Connect and say hello in chat"
```

### Chat Bot
```
open_client dc hub.example.com:411 --nickname chatbot --description "NetGet Chat Bot" "Monitor chat and respond to questions about NetGet"
```

### Search Agent
```
open_client dc hub.example.com:411 --nickname searcher "Search for 'ubuntu iso' and report all results"
```

### Hub Monitor
```
open_client dc hub.example.com:411 --nickname monitor "Join hub, get user list, monitor for new users joining/leaving"
```

## Performance Characteristics

### Latency
- **Connection + Auth**: 3-5 RTT (Connect → Lock → Key/ValidateNick/MyINFO → Hello)
- **Chat Message**: 1 RTT (Send → Hub broadcast)
- **Search**: Variable (depends on hub size and results)

### Throughput
- **Messages**: Lightweight text protocol, hundreds of messages/sec possible
- **Concurrent Hubs**: Client can connect to multiple hubs simultaneously (separate client instances)
- **LLM Integration**: Messages processed as they arrive (no batching)

### Scripting Compatibility

**Good candidate for scripting**:
- Repetitive patterns (auto-respond to greetings, forward searches)
- State-based logic (only chat when authenticated)
- Text parsing (easy for scripting to handle)
- Rule-based responses (if message contains X, reply with Y)

## NMDC Protocol References

### Common Messages (Client → Hub)

- **$ValidateNick <nick>** - Request nickname validation
- **$Key <key>** - Respond to lock challenge
- **$Version <version>** - Client version (e.g., "1,0091")
- **$MyINFO $ALL <nick> <desc>$ $<conn><flag>$<email>$<size>$** - User information
- **`<nick> message`** - Public chat message
- **$To: <target> From: <source> $`<source>` message** - Private message
- **$Search Hub:<nick> <params>?<query>** - Search for files
- **$GetNickList** - Request user list
- **$Quit** - Notify disconnection

### Common Messages (Hub → Client)

- **$Lock <lock> Pk=<pk>** - Authentication challenge
- **$Hello <nick>** - Accept user login
- **$HubName <name>** - Hub name
- **$HubTopic <topic>** - Hub topic
- **$NickList <nick>$$<nick>$$...** - List of connected users
- **$SR <source> <file>\x05<size> <slots>\x05<hub>** - Search result
- **$Kick <nick>** - Force disconnect user
- **$ForceMove <address>** - Redirect to another hub
- **`<nick> message`** - Broadcast chat message

### Message Format

All commands end with `|` (pipe character).

Examples:
- `$Lock EXTENDEDPROTOCOLABCABC Pk=MyHub|`
- `$ValidateNick alice|`
- `$Hello alice|`
- `<alice> Hello everyone!|`
- `$To: bob From: alice $<alice> Hi bob!|`

## Testing Strategy

See `tests/client/dc/CLAUDE.md` for comprehensive testing documentation.

**Test Approach**:
- Use local DC server from `src/server/dc/` as test target
- Mock LLM responses for predictable behavior
- Test authentication flow, chat, search, user list
- < 10 LLM calls total

## References

- [NMDC Protocol Specification](https://nmdc.sourceforge.io/NMDC.html)
- [ADC Protocol](https://adc.sourceforge.io/ADC.html)
- [DC++ Official Site](https://dcplusplus.sourceforge.io/)
- Server implementation: `src/server/dc/CLAUDE.md`
- Existing client patterns: `src/client/tcp/`, `src/client/redis/`
