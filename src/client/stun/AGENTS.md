# STUN Client Implementation

## Overview

The STUN (Session Traversal Utilities for NAT) client implementation provides LLM-controlled NAT traversal discovery.
The LLM can send binding requests to STUN servers and interpret the external IP address and port discovered.

## Implementation Details

### Library Choice

- **stunclient** - Simple UDP-only STUN client for resolving external IP:port behind NAT
- Supports both sync and async (Tokio) operations
- Implements RFC 5389 (Session Traversal Utilities for NAT)
- Simple API: bind UDP socket, query external address

### Architecture

```
┌──────────────────────────────────────────┐
│  StunClient::connect_with_llm_actions    │
│  - Bind UDP socket (0.0.0.0:0)           │
│  - Store STUN server in protocol_data    │
│  - Call LLM with connected event         │
│  - Mark as Connected                     │
└──────────────────────────────────────────┘
         │
         ├─► send_binding_request() - Called per LLM action
         │   - Bind new UDP socket
         │   - Query STUN server via stunclient
         │   - Extract XOR-MAPPED-ADDRESS
         │   - Call LLM with binding response event
         │   - Update memory
         │
         └─► Background Monitor Task
             - Checks if client still exists
             - Exits if client removed
```

### Connection Model

STUN client is **request/response** based over UDP:

- "Connection" = initialization and UDP socket binding
- Each binding request is independent (new UDP socket)
- LLM triggers binding requests via actions
- Responses trigger LLM calls for interpretation
- No persistent connection state

### LLM Control

**Async Actions** (user-triggered):

- `send_binding_request` - Send STUN binding request
    - No parameters needed
    - Returns Custom result triggering binding request
- `disconnect` - Stop STUN client

**Sync Actions** (in response to binding responses):

- `send_binding_request` - Send another binding request to refresh
- `wait_for_more` - Wait before sending next request

**Events:**

- `stun_connected` - Fired when client initialized and UDP socket bound
    - Data includes: local_addr, stun_server
- `stun_binding_response` - Fired when binding response received
    - Data includes: external_ip, external_port, external_addr, local_addr, stun_server

### Structured Actions (CRITICAL)

STUN client uses **structured data**, NOT raw bytes:

```json
// Binding request action
{
  "type": "send_binding_request"
}

// Binding response event
{
  "event_type": "stun_binding_response",
  "data": {
    "external_ip": "203.0.113.45",
    "external_port": 54321,
    "external_addr": "203.0.113.45:54321",
    "local_addr": "0.0.0.0:12345",
    "stun_server": "stun.l.google.com:19302"
  }
}
```

LLMs can request binding queries and interpret the NAT mapping information.

### Request Flow

1. **Initialization**: UDP socket bound, LLM receives `stun_connected` event
2. **LLM Action**: `send_binding_request`
3. **Action Execution**: Returns `ClientActionResult::Custom`
4. **Binding Request**:
    - Create new UDP socket
    - Use `stunclient::StunClient::new(stun_server)`
    - Call `query_external_address_async(&udp_socket)`
    - Extract XOR-MAPPED-ADDRESS from response
5. **Response Handling**:
    - Parse external IP and port
    - Create `stun_binding_response` event
    - Call LLM for interpretation
6. **LLM Response**: May trigger follow-up binding requests or disconnect

### Startup Parameters

None required - STUN client only needs server address in remote_addr.

### Dual Logging

```rust
info!("STUN client {} sending binding request to {}", client_id, stun_server);  // → netget.log
status_tx.send("[CLIENT] STUN binding request sent");                           // → TUI
```

### Error Handling

- **Connection Failed**: UDP bind error, client not created
- **Binding Request Failed**: Log error, return Err, don't crash client
    - DNS resolution failure
    - Network timeout
    - Invalid STUN response
- **LLM Error**: Log, continue accepting actions

## Features

### Supported Operations

- Binding requests (RFC 5389)
- XOR-MAPPED-ADDRESS attribute parsing
- IPv4 support (primary)
- IPv6 support (if STUN server supports)

### Protocol Details

- **Transport**: UDP
- **Port**: STUN servers typically use port 3478 or 19302 (Google STUN)
- **Message Type**: Binding Request (0x0001)
- **Response Type**: Binding Success Response (0x0101)

### Public STUN Servers

Google STUN servers (free, reliable):

- `stun.l.google.com:19302`
- `stun1.l.google.com:19302`
- `stun2.l.google.com:19302`
- `stun3.l.google.com:19302`
- `stun4.l.google.com:19302`

## Limitations

- **UDP Only** - No TCP STUN support
- **Basic Binding Only** - No RFC 5780 NAT Behavior Discovery
- **No Authentication** - Long-term credentials not supported
- **No TURN** - Only STUN, not TURN relay functionality
- **Single Server** - One STUN server per client instance
- **No Connection Reuse** - Each binding request creates new UDP socket

## Usage Examples

### Discover External IP

**User**: "Connect to stun.l.google.com:19302 and discover my external IP address"

**Flow**:

1. Client initializes, binds UDP socket
2. LLM receives `stun_connected` event
3. LLM sends `send_binding_request` action
4. Binding request executed, response received
5. LLM receives `stun_binding_response` with external address
6. LLM reports: "Your external IP is 203.0.113.45:54321"

**LLM Actions**:

```json
// Initial action after connection
{
  "type": "send_binding_request"
}

// Response interpretation (memory update)
// LLM: "External address discovered: 203.0.113.45:54321"
```

### Periodic NAT Binding Refresh

**User**: "Monitor my external IP every 30 seconds"

**Flow**:

1. LLM sends initial binding request
2. Receives response with external address
3. Waits (via scheduled task or LLM decision)
4. Sends another binding request
5. Compares addresses to detect NAT rebinding

**LLM Actions**:

```json
// First request
{ "type": "send_binding_request" }

// After response
{ "type": "wait_for_more" }

// After delay (via scheduled task or user prompt)
{ "type": "send_binding_request" }
```

### NAT Type Detection

**User**: "Determine my NAT type"

**Flow**:

1. Send binding request to STUN server
2. Compare external vs local address
3. Determine NAT type based on mapping

**Analysis**:

- External == Local: No NAT
- External != Local: Behind NAT
- (Advanced: RFC 5780 required for full NAT type detection)

## Testing Strategy

See `tests/client/stun/CLAUDE.md` for E2E testing approach.

Key points:

- Use Google STUN servers (stun.l.google.com:19302)
- Test binding request/response flow
- Verify external address extraction
- Minimal LLM calls (< 10)

## Future Enhancements

- **RFC 5780 Support** - NAT Behavior Discovery
    - Detect NAT type (Full Cone, Restricted, Port Restricted, Symmetric)
    - CHANGE-REQUEST attribute
    - OTHER-ADDRESS attribute
- **Long-term Credentials** - MESSAGE-INTEGRITY support
- **TCP STUN** - TCP transport option
- **TURN Integration** - Relay functionality for WebRTC
- **IPv6 Support** - Full IPv6 binding requests
- **Connection Reuse** - Keep UDP socket alive for multiple queries
- **Rate Limiting** - Prevent STUN server abuse

## Command channel (injected actions)

The client registers a `command_support` command channel, so `AppState::send_to_client` — the
dashboard's `[ send ]` — can execute an action inside the running client. The channel is
registered **before** the `stun_connected` LLM call, which a `*` → manual routing rule can park
for minutes.

The command loop **replaced** the bare 5-second "has the client been removed yet?" watchdog
task described above. It still performs that check, on a `tokio::select!` timer arm.

| Action | `ClientSendOutcome` |
|---|---|
| `send_binding_request` | `Executed { detail }` — see below |
| `wait_for_more` | `Executed` |
| `disconnect` | `Disconnected`; the command loop ends and the handle is dropped |
| anything else | `Rejected { error }` |

**Why a binding request reports `Executed` and not `Sent` — a real gap, stated rather than
papered over.** The exchange runs inside `stunclient`, which binds and owns its own UDP socket
(see "No Connection Reuse" above) and reports no byte count. There is no number NetGet could
hand back that would be true, so it hands back the discovered external address instead:
`binding exchange completed via stunclient: external address 203.0.113.45:54321 (byte count not
observable - stunclient owns the socket)`. Closing this would mean replacing `stunclient` with
a hand-rolled RFC 5389 encoder over NetGet's own socket — a protocol rewrite, not a
command-channel change.

`send_binding_request` is now `query_external_address` (run the exchange) plus
`report_binding_response` (fire the `stun_binding_response` event). The split lets the command
loop answer its caller as soon as the exchange completes and report to the model afterwards, so
a manual rule parked on that event cannot hold `[ send ]` open for its whole timeout. The
public `send_binding_request` is the composition of the two and behaves as before.
