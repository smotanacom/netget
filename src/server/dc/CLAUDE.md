# DC (Direct Connect) Protocol Implementation

## Overview

DC (Direct Connect) is a peer-to-peer file sharing protocol where a central hub coordinates client connections. This
implementation focuses on the NMDC (Neo-Modus Direct Connect) protocol, which is simpler and more widely used than the
newer ADC protocol. The LLM controls authentication, search responses, chat messages, and hub management.

**Status**: Experimental (Application Protocol)
**RFC**: No official RFC (community-maintained specification)
**Specification**: https://nmdc.sourceforge.io/NMDC.html
**Port**: 411 (plain TCP), 412 (with TLS, not implemented)

## Library Choices

- **No DC library** - Manual protocol implementation
    - NMDC is line-based text protocol (simple parsing)
    - Commands prefixed with `$`, terminated with `|` (pipe)
    - Responses constructed as formatted strings
    - No complex binary protocol
- **tokio** - Async runtime and I/O
    - `TcpListener` for accepting connections
    - `BufReader` for reading until pipe delimiter
    - `AsyncWriteExt` for sending responses

**Rationale**: The NMDC protocol is simple enough that no dedicated library is needed. Messages are text terminated with
`|`, commands start with `$`. Manual implementation gives LLM full control and avoids library constraints. No existing
Rust DC library was found on crates.io.

## Architecture Decisions

### 1. NMDC Protocol Choice

We implement NMDC instead of ADC because:

- NMDC is simpler and more widely supported
- Most DC++ clients support NMDC
- Text-based protocol is easier for LLM to understand
- ADC can be added later as an enhancement

### 2. Hub-Centric Architecture

The server acts as a "hub" that:

- Accepts client connections
- Manages lock/key authentication handshake
- Routes chat messages between clients
- Routes search requests and results
- Broadcasts user information
- Facilitates peer-to-peer connections via ConnectToMe

### 3. Action-Based LLM Control

The LLM receives NMDC commands and responds with structured actions:

- `send_dc_lock` - Send $Lock challenge for authentication
- `send_dc_hello` - Accept user with $Hello
- `send_dc_hubname` - Send hub name
- `send_dc_message` - Send chat message to specific user
- `send_dc_broadcast` - Broadcast message to all users
- `send_dc_userlist` - Send list of connected users
- `send_dc_search_result` - Send search result
- `send_dc_kick` - Kick user from hub
- `send_dc_redirect` - Redirect user to another hub
- `send_dc_raw` - Send raw NMDC command
- `wait_for_more` - Answer with nothing and keep the connection open. It does **not** buffer
  anything: the hub raises an event only after the closing `|`, so the model is never shown a
  partial command and there is no accumulating state to enter. What it is for is the many NMDC
  commands a hub does not reply to ($Version, $MyINFO, $GetINFO)
- `close_connection` - Close this client's connection

### 4. Pipe-Delimited Message Processing

DC message flow:

1. Accept TCP connection
2. Read bytes until `|` delimiter
3. Parse command (e.g., "$ValidateNick alice|")
4. Send command to LLM as `dc_command_received` event
5. LLM returns actions (e.g., `send_dc_lock`)
6. Execute actions (send responses)
7. Loop for next command

### 5. Automatic Pipe Termination

All NMDC commands must end with `|`:

- Received messages: Parsed up to and including `|`
- Sent messages: Automatically add `|` if not present
- All send actions ensure `|` termination

### 6. Lock/Key Authentication

NMDC uses a challenge-response authentication:

1. Hub sends `$Lock <lock_string> Pk=<pk_string>|`
2. Client calculates key from lock and sends `$Key <key_string>|`
3. Hub validates key (LLM can implement validation logic)
4. Hub sends `$Hello <nickname>|` to accept

**Note**: The key calculation algorithm is well-defined but not implemented server-side. LLM can choose to accept all
keys or implement proper validation.

### 7. Client State Tracking

`DcProtocol` maintains per-connection state:

```rust
struct DcClientState {
    nickname: Option<String>,
    description: Option<String>,
    email: Option<String>,
    share_size: u64,
    is_operator: bool,
}
```

State is used for:

- Tracking connected users for $NickList
- Identifying operators
- Managing chat routing
- Generating user information broadcasts

### 8. Dual Logging

- **DEBUG**: Command summary with preview ("DC received 20 bytes: $ValidateNick alice")
- **TRACE**: Full command text ("DC command: $ValidateNick alice|")
- Both go to netget.log and TUI Status panel

### 9. Connection Management

Each DC client gets:

- Unique `ConnectionId`
- Entry in `ServerInstance.connections` with `ProtocolConnectionInfo::Dc`
- Tracked bytes sent/received, packets sent/received
- State: Active until client disconnects or LLM kicks

## LLM Integration

### Event Types

#### `dc_command_received`

Triggered when DC client sends a command.

Event parameters:

- `command` (string) - The NMDC command received (without `|` terminator)
- `client_nickname` (string, optional) - Client's nickname if set
- `command_type` (string) - Parsed command type (e.g., "ValidateNick", "MyINFO", "Search")

### Available Actions

#### `send_dc_lock`

Send $Lock challenge for authentication.

Parameters:

- `lock` (optional) - Lock string (default: random string)
- `pk` (optional) - PK string (default: "NetGetHub")

Example:

```json
{
  "type": "send_dc_lock",
  "lock": "EXTENDEDPROTOCOLABCABCABCABCABCABC",
  "pk": "NetGetHub"
}
```

Sends: `$Lock EXTENDEDPROTOCOLABCABCABCABCABCABC Pk=NetGetHub|`

#### `send_dc_hello`

Accept user with $Hello.

Parameters:

- `nickname` (required) - Client nickname to accept

Example:

```json
{
  "type": "send_dc_hello",
  "nickname": "alice"
}
```

Sends: `$Hello alice|`

#### `send_dc_hubname`

Send hub name to client.

Parameters:

- `name` (required) - Hub name

Example:

```json
{
  "type": "send_dc_hubname",
  "name": "NetGet DC Hub"
}
```

Sends: `$HubName NetGet DC Hub|`

#### `send_dc_message`

Send chat message from hub or user to specific client.

Parameters:

- `target` (required) - Target nickname
- `source` (optional) - Source nickname (default: hub)
- `message` (required) - Message text

Example:

```json
{
  "type": "send_dc_message",
  "target": "alice",
  "source": "HubBot",
  "message": "Welcome to the hub!"
}
```

Sends: `$To: alice From: HubBot $<HubBot> Welcome to the hub!|`

#### `send_dc_broadcast`

Broadcast message to all connected clients.

Parameters:

- `source` (required) - Source nickname or hub name
- `message` (required) - Message text

Example:

```json
{
  "type": "send_dc_broadcast",
  "source": "Hub",
  "message": "Server maintenance in 5 minutes"
}
```

Sends to all: `<Hub> Server maintenance in 5 minutes|`

#### `send_dc_userlist`

Send list of connected users.

Parameters:

- `users` (required) - Array of nicknames

Example:

```json
{
  "type": "send_dc_userlist",
  "users": ["alice", "bob", "charlie"]
}
```

Sends: `$NickList alice$$bob$$charlie$$|`

#### `send_dc_search_result`

Send search result to client.

Parameters:

- `source` (required) - Source nickname (who has the file)
- `filename` (required) - File name
- `size` (required) - File size in bytes
- `slots` (optional) - Available slots (default: 1)
- `hub_name` (optional) - Hub name (default: current hub)

Example:

```json
{
  "type": "send_dc_search_result",
  "source": "bob",
  "filename": "ubuntu-22.04.iso",
  "size": 3654957056,
  "slots": 2,
  "hub_name": "NetGetHub"
}
```

Sends: `$SR bob ubuntu-22.04.iso\x053654957056 2/2\x05NetGetHub|`

#### `send_dc_kick`

Kick user from hub.

Parameters:

- `nickname` (required) - User to kick

Example:

```json
{
  "type": "send_dc_kick",
  "nickname": "alice"
}
```

Sends: `$Kick alice|`

#### `send_dc_redirect`

Redirect user to another hub.

Parameters:

- `address` (required) - Target hub address (host:port)

Example:

```json
{
  "type": "send_dc_redirect",
  "address": "hub.example.com:411"
}
```

Sends: `$ForceMove hub.example.com:411|`

#### `send_dc_raw`

Send raw NMDC command (for advanced use).

Parameters:

- `command` (required) - Raw command (auto-adds `|` if missing)

Example:

```json
{
  "type": "send_dc_raw",
  "command": "$HubTopic Welcome to NetGet!"
}
```

Sends: `$HubTopic Welcome to NetGet!|`

See `actions.rs` for complete action list.

## Connection Management

### Connection Lifecycle

1. **Accept**: TCP listener accepts connection
2. **Register**: Connection added to `ServerInstance` with `ProtocolConnectionInfo::Dc`
3. **Split**: Stream split into ReadHalf and WriteHalf
4. **Track**: WriteHalf stored in `Arc<Mutex<WriteHalf>>` for sending
5. **Send Lock**: Hub immediately sends $Lock challenge
6. **Send HubName**: only when `hub_name` / `hub_topic` was configured — see below
7. **Read Loop**: Continuous pipe-delimited reading until disconnect
8. **Close**: Connection removed when client closes or LLM kicks

### Startup parameters: `hub_name` and `hub_topic`

Both are declared, and both were read by nothing — the only identity a client ever saw was
the hardcoded `Pk=NetGetHub` inside `$Lock`, so setting either changed nothing observable.
They are now resolved once at spawn by `hub_name_command()` into the `$HubName` the hub sends
straight after `$Lock`, which is the first thing a DC++ client displays:

| configured | on the wire |
|---|---|
| neither | nothing — byte-for-byte the old behaviour |
| `hub_name` only | `$HubName <name>\|` |
| `hub_topic` only | `$HubName NetGetHub - <topic>\|` |
| both | `$HubName <name> - <topic>\|` |

`<name> - <topic>` is how NMDC carries a topic: the base protocol has no separate topic
command, and DC++ splits the name at the first ` - `. (`$HubTopic` exists as an extension and
the model can still send it with `send_dc_raw`.) A topic with no name uses `NetGetHub` — the
same `Pk=` value already in `$Lock` — rather than presenting the topic as the hub's name.

Both values are sanitised for `|`, `\r` and `\n` before use. NMDC frames commands by `|`, so
an unescaped pipe in either would terminate the command early and leave the remainder to be
parsed as a second one.

### State Management

- `ProtocolState`: Idle/Processing/Accumulating (prevents concurrent LLM calls)
- `queued_data`: Data buffered while LLM is processing
- Connection stays in ServerInstance until closed
- UI updates on every message (bytes sent/received, last activity)

### 10. Dashboard Injection (peer handle)

Every connection registers a peer handle (`peer_support::register_peer_channel` +
`spawn_peer_command_task`) right after it is tracked and before the `$Lock` goes out, so the
dashboard's `[ message this peer ]` / `[ disconnect this peer ]` work from the first moment.
All wire verbs return `ActionResult::Output`, so the generic peer task covers the whole
vocabulary; `close_connection` returns `CloseConnection`, which half-closes the socket. The
handle is removed on every exit path through the single cleanup after `run_connection`.
Byte/packet counters are updated on every read and on every write (the `$Lock`, the LLM/handler
replies). Test: `tests/server/dc/peer_inject_test.rs` (zero LLM calls).

### 11. Command framing: the bound and the escaping

Two things about `|` that were missing and are worth keeping straight.

**Reading.** The accumulator grows a byte at a time until a `|` arrives. It had no cap and no
timeout, so an **unauthenticated** peer that connects and never sends one grew it as fast as its
link allowed until the process died — `nc host 411 < /dev/zero` was the whole attack. It is now
bounded by `MAX_COMMAND_LEN` (64 KiB; the largest real command, a `$MyINFO` with a long
description, stays well under a kilobyte), and exceeding it closes the connection with
`decision=oversize_command`. The read half is also wrapped in a `BufReader`: the loop reads one
byte at a time, and unbuffered that was one syscall per byte of every command.

**Writing.** Every command is built as `format!("$Something {}|", value)`, so a `|` inside any
interpolated value ends the command early and the client parses the remainder as a command of
its own. `hub_name_command` had always stripped `|`, `\r` and `\n` from its two startup
parameters; the ten action executors had not, so `{"message": "hi|$Kick alice"}` was two
commands. `nmdc_field()` now does it for all of them. `send_dc_raw` is deliberately exempt —
sending an arbitrary frame is its declared purpose.

Tests: `test_oversize_command_is_refused_not_buffered` (no LLM call) and
`test_pipe_in_a_message_cannot_inject_a_second_command`.

## Known Limitations

### 1. No Key Validation

- Server doesn't validate $Key responses to $Lock challenges
- LLM can accept all keys or implement custom validation
- No enforcement of lock/key algorithm

**Workaround**: LLM can implement validation in prompt logic.

### 2. No Hub State Management

- Server doesn't track all users in a central list
- No automatic $NickList generation
- No channel/room support
- LLM must track hub state in conversation context

**Workaround**: LLM maintains pseudo-state through prompts and memory.

### 3. No Peer-to-Peer Connection Handling

- Server only handles hub protocol
- $ConnectToMe routing not implemented
- No direct client-to-client file transfers
- Hub only facilitates discovery

**Future Enhancement**: Add P2P connection tracking and routing.

### 4. Limited Search Support

- No search index
- No file listing
- LLM generates search results from prompts
- No integration with actual file system

### 5. No Operator Commands

- No /kick, /ban commands from operators
- No operator privilege enforcement
- LLM must implement operator logic

### 6. No TLS Support

- Plain TCP only (port 411)
- No SSL/TLS encryption (port 412)

**Workaround**: Use reverse proxy (e.g., nginx) for TLS termination.

### 7. No ADC Protocol Support

- NMDC only
- ADC (Advanced Direct Connect) not implemented
- No protocol negotiation

**Future Enhancement**: Add ADC support.

### 8. No EXTENDEDPROTOCOL Features

- Basic NMDC only
- No $Supports negotiation
- No compression (ZLIB)
- No UTF-8 support

## Example Prompts

### Basic DC Hub

```
listen on port 411 via dc
When users send $ValidateNick, send $Lock challenge
When users send $Key, accept with $Hello and send hub name "NetGet Hub"
When users send $MyINFO, acknowledge
Send welcome message to new users
```

### Chat Hub

```
listen on port 411 via dc
Accept all users with $Hello
When users send chat messages, broadcast to all connected users
Support private messages with $To/$From syntax
```

### Search Hub

```
listen on port 411 via dc
Accept all users
When users send $Search commands, generate fake search results
Return 2-3 search results for any query
Use filenames like "document.pdf", "music.mp3", "movie.avi"
```

### Secure Hub

```
listen on port 411 via dc
When users send $ValidateNick, check if nickname is allowed (alice, bob, charlie only)
When users send $Key, validate it matches the lock
Kick users who fail validation
Send welcome message to authenticated users
```

## Performance Characteristics

### Latency

- **Per Command (with scripting)**: Sub-millisecond
- **Per Command (without scripting)**: 2-5 seconds (LLM call)
- Command parsing: <1 microsecond
- Message formatting: <1 microsecond

### Throughput

- **With Scripting**: Thousands of commands per second
- **Without Scripting**: ~0.2-0.5 commands per second (LLM-limited)
- Concurrent connections: Unlimited (bounded by system resources)
- Each connection processes independently

### Scripting Compatibility

Good scripting candidate:

- Text-based protocol (easy to parse and generate)
- Repetitive command/response patterns
- State can be maintained in script
- High message volume typical for chat and search

## NMDC Protocol References

### Common Commands (Client → Hub)

- **$ValidateNick <nick>** - Request nickname validation
- **$Key <key>** - Respond to lock challenge
- **$Version <version>** - Client version
- **$MyINFO** - User information (description, email, share size)
- **`<nick> message`** - Public chat message
- **$To: <target> From: <source> $`<source>` message** - Private message
- **$Search** - Search for files
- **$ConnectToMe <remote> <ip>:<port>** - Request P2P connection
- **$Quit <nick>** - Notify disconnection

### Common Commands (Hub → Client)

- **$Lock <lock> Pk=<pk>** - Authentication challenge
- **$Hello <nick>** - Accept user login
- **$HubName <name>** - Hub name
- **$HubTopic <topic>** - Hub topic
- **$NickList <nick>$$<nick>$$...** - List of connected users
- **$OpList <nick>$$<nick>$$...** - List of operators
- **$Quit <nick>** - User disconnected notification
- **$Kick <nick>** - Force disconnect user
- **$ForceMove <address>** - Redirect to another hub
- **$SR <source> <result>** - Search result
- **`<nick> message`** - Broadcast chat message

### Message Format

All commands end with `|` (pipe character).

Examples:

- `$Lock EXTENDEDPROTOCOLABCABC Pk=NetGetHub|`
- `$ValidateNick alice|`
- `$Hello alice|`
- `<alice> Hello everyone!|`
- `$To: bob From: alice $<alice> Hi bob!|`

## References

- [NMDC Protocol Specification](https://nmdc.sourceforge.io/NMDC.html)
- [ADC Protocol](https://adc.sourceforge.io/ADC.html)
- [DC++ Official Site](https://dcplusplus.sourceforge.io/)
- [PtokaX DC Protocol Wiki](http://wiki.ptokax.org/doku.php?id=dcprotocol)
