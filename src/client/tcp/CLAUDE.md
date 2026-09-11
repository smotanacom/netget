# TCP Client Implementation

## Overview

The TCP client implementation provides LLM-controlled outbound TCP connections. The LLM can connect to TCP servers, send
raw bytes, and interpret responses.

## Implementation Details

### Library Choice

- **tokio::net::TcpStream** - Async TCP client
- Direct socket I/O with hex encoding for LLM interaction
- Split stream pattern for concurrent read/write

### Architecture

```
┌────────────────────────────────────────┐
│  TcpClient::connect_with_llm_actions   │
│  - Connect to remote address           │
│  - Split stream (read/write)           │
│  - Spawn read loop                     │
└────────────────────────────────────────┘
         │
         ├─► Read Loop
         │   - Read data from server
         │   - Call LLM with data_received event
         │   - Execute actions (send_data, disconnect)
         │   - State machine (Idle/Processing/Accumulating)
         │
         └─► Write Half (Arc<Mutex<WriteHalf>>)
             - Shared for sending data
             - Used by action execution
```

### No connection state machine — and there never was a working one

This client reads and calls the model on the **same task**: `read_half.read()` is not called
again until the LLM round-trip returns, so data arriving during a call waits in the kernel's
socket buffer. That is the same discipline `reverse_shell` documents.

The Idle/Processing/Accumulating enum that used to live here was copied from the *server*
(`src/server/tcp/mod.rs`), where it is real because the reader task hands each payload to a
separate task and a second read genuinely can arrive mid-call. Here `Processing` and
`Accumulating` were unreachable, and the `queued_data` their branches filled was cleared without
ever being read. It has been removed: dead code that reads as backpressure is worse than no code.

`ClientData` now holds the LLM memory and nothing else.

### Locking rule, and the trap this client fell into

Never pass `&client_data.lock().await.memory` into `call_llm_for_client`. A temporary in a
`match` scrutinee lives until the end of the whole `match`, so the guard stays held inside the
arms — and the `Ok` arm re-locks the same non-reentrant tokio `Mutex` to store `memory_updates`.
Copy the memory out first, as the code now does. It is latent here only because
`call_llm_for_client` currently hardcodes `memory_updates = None`; the DC client had the same
shape with a re-lock on every action, and that one deadlocked for real.

### LLM Control

**Async Actions** (user-triggered):

- `send_tcp_data` - Send hex-encoded bytes to server
- `disconnect` - Close connection

**Sync Actions** (in response to received data):

- `send_tcp_data` - Send bytes as response
- `wait_for_more` - Don't respond yet, accumulate data

**Events:**

- `tcp_connected` - Fired when connection established
- `tcp_data_received` - Fired when data received from server

### Data Encoding

**Critical**: received data is always hex-encoded; outbound data has two fields and they are not
interchangeable.

- Received: `{"data_hex": "48656c6c6f", "data_length": 5}`
- Sent as text: `{"type": "send_tcp_data", "data": "HELLO\r\n"}` — the characters go on the wire
- Sent as binary: `{"type": "send_tcp_data", "data_hex": "776f726c64"}` — decoded first

`data` is checked first, so putting hex in it sends the hex digits themselves. `tcp_connected`'s
own worked example used to be `{"data": "48656c6c6f"}`, which taught the model to do exactly
that — the same confusion the TCP *server* was given an explicit `encoding` field to end. To
echo, pass the event's `data_hex` back as `data_hex`.

### Dual Logging

```rust
info!("TCP client {} connected", client_id);           // → netget.log
status_tx.send("[CLIENT] TCP client connected");      // → TUI
```

### Connection Lifecycle

1. **Connect**: `TcpStream::connect(remote_addr)`
2. **Connected**: Update ClientStatus::Connected
3. **Data Flow**: Read loop processes incoming data
4. **Disconnect**: ConnectionStatus::Disconnected or Error

### Error Handling

- **Connection Failed**: Return error, client stays in Error state
- **Read Error**: Log, update status to Error, break loop
- **Write Error**: Log, connection may close
- **LLM Error**: Log, continue accepting data

### Command channel (injected actions)

The read loop carries a `tokio::select!` arm on a bounded command channel
(`client/command_support.rs`), registered via
`AppState::register_client_handle`. `AppState::send_to_client(client_id,
action, timeout)` executes an action (`send_tcp_data`, `disconnect`,
`wait_for_more`) inside the loop exactly as LLM-produced actions run — this is
what the dashboard's [send] button calls, and it needs no LLM. Each injected
action is recorded in the access log (owner = client, event
`injected_action`). On loop exit the handle is dropped so later sends fail
fast. E2E: `tests/client_handle_test.rs`.

## Limitations

- **No TLS Support** - Raw TCP only (TLS could be added later)
- **No Reconnection** - Must manually reconnect via action
- **No Buffering Control** - Uses default 8KB buffer
- **Hex Encoding Overhead** - 2x data size for LLM interaction

## Testing Strategy

See `tests/client/tcp/CLAUDE.md` for E2E testing approach.
