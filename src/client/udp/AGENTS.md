# UDP Client Implementation

## Overview

The UDP client provides LLM-controlled connectionless datagram communication. Unlike TCP, UDP has no persistent
connection - the client binds a local socket and sends/receives datagrams to/from any address.

## Library Choices

**Core Library:** `tokio::net::UdpSocket`

- Part of Tokio async runtime
- Provides `send_to()` and `recv_from()` for datagram I/O
- No external dependencies beyond Tokio
- Connectionless - can send to any address, receive from any source

**Encoding:** `hex` crate for LLM-friendly data representation

- LLMs work with hex strings, not raw bytes
- Ensures reliable data transmission without binary confusion

## Architecture

### Connection Model

**"Connection" is a misnomer for UDP:**

- UDP is connectionless - no handshake, no stream, no guaranteed delivery
- "Connecting" means binding a local socket and setting a default target address
- Client can send to any address, not just the default target
- Client receives from any source that sends to the bound port

**Lifecycle:**

1. **Bind:** `UdpSocket::bind("0.0.0.0:0")` binds to any available local port
2. **Active:** Socket remains open, listening for incoming datagrams
3. **Send:** LLM can send datagrams to any address via `send_udp_datagram` action
4. **Receive:** Any incoming datagram triggers LLM with `udp_datagram_received` event
5. **Close:** LLM closes socket via `close_socket` action

### State Management

**Per-Client State Machine:**

```
Idle → (datagram received) → Processing → (LLM responds) → Idle
                              ↓
                         (wait_for_more)
                              ↓
                        Accumulating → (more datagrams) → Accumulating → (LLM responds) → Idle
```

**State Fields:**

- `state`: Idle, Processing, or Accumulating
- `queued_datagrams`: Vec of (data, source_addr) tuples for datagrams received during Processing
- `memory`: LLM's persistent memory across events
- `default_target`: Default address for sending (from initial remote_addr)

**Queueing Strategy:**

- **Idle:** Process datagram immediately
- **Processing:** Queue datagram for later processing
- **Accumulating:** Queue datagram and wait for LLM decision (wait_for_more or respond)

### Data Flow

```
Datagram arrives → Check state:
  - Idle: Process immediately with LLM
  - Processing: Queue datagram
  - Accumulating: Queue datagram

LLM responds with actions:
  - send_udp_datagram: Send datagram to target address
  - change_target: Update default target address
  - wait_for_more: Switch to Accumulating state
  - close_socket: Close UDP socket

After LLM response:
  - If queue not empty: Process next datagram
  - Else: Return to Idle
```

## LLM Integration

### Events

**1. `udp_connected` Event:**
Triggered when socket is bound and ready.

```json
{
  "remote_addr": "127.0.0.1:8080",
  "local_addr": "0.0.0.0:54321"
}
```

LLM can send initial datagrams or wait for server response.

**2. `udp_datagram_received` Event:**
Triggered when a datagram is received.

```json
{
  "data_hex": "48656c6c6f",
  "data_length": 5,
  "source_addr": "127.0.0.1:8080"
}
```

LLM interprets the data and decides how to respond.

### Actions

**Async Actions (User-Triggered):**

1. **send_udp_datagram**
   ```json
   {
     "type": "send_udp_datagram",
     "data_hex": "48656c6c6f",
     "target_addr": "127.0.0.1:8080"  // Optional, defaults to default_target
   }
   ```
   Send a datagram to any address.

2. **change_target**
   ```json
   {
     "type": "change_target",
     "new_target": "127.0.0.1:9090"
   }
   ```
   Change the default target address.

3. **close_socket**
   ```json
   {
     "type": "close_socket"
   }
   ```
   Close the UDP socket.

**Sync Actions (Response to Events):**

1. **send_udp_datagram** (same as async)
    - If `target_addr` omitted, defaults to `source_addr` of received datagram

2. **wait_for_more**
   ```json
   {
     "type": "wait_for_more"
   }
   ```
   Accumulate more datagrams before responding.

### Action Execution

**Custom Action Pattern:**
UDP uses `ClientActionResult::Custom` for `send_udp_datagram` and `change_target` because these actions need additional
parameters beyond just sending bytes.

```rust
ClientActionResult::Custom {
    name: "send_udp_datagram",
    data: json!({
        "data": vec![0x48, 0x65, 0x6c, 0x6c, 0x6f],
        "target_addr": Some("127.0.0.1:8080"),
    })
}
```

The `handle_llm_result` function parses this and calls `socket.send_to()`.

## UDP vs TCP Differences

| Aspect          | TCP Client                       | UDP Client                                     |
|-----------------|----------------------------------|------------------------------------------------|
| **Connection**  | Stream-oriented, persistent      | Connectionless, datagram-based                 |
| **State**       | Connected/Disconnected           | Socket bound/closed                            |
| **Send**        | `write()` to stream              | `send_to(addr)` for each datagram              |
| **Receive**     | `read()` from stream             | `recv_from()` returns (data, source)           |
| **Target**      | Fixed remote address             | Can send to any address                        |
| **Reliability** | Guaranteed delivery, ordered     | Best-effort, unordered                         |
| **LLM Actions** | send_tcp_data, disconnect        | send_udp_datagram, change_target, close_socket |
| **Events**      | tcp_connected, tcp_data_received | udp_connected, udp_datagram_received           |

## Limitations

1. **No Connection State:** UDP has no connection, so "disconnect" events don't exist. Socket is either bound or closed.

2. **Unreliable Delivery:** Datagrams may be lost, duplicated, or reordered. LLM must handle this (e.g., retries,
   sequence numbers).

3. **Size Limit:** UDP datagrams are limited to 65,535 bytes (minus IP/UDP headers). In practice, stay under ~1400 bytes
   to avoid IP fragmentation.

4. **Multiple Sources:** Client may receive from multiple sources. LLM must track `source_addr` to respond correctly.

5. **Timeouts:** UDP has no built-in timeout mechanism. Use scheduled tasks for timeout handling if needed.

6. **No Flow Control:** No backpressure mechanism. Fast senders can overwhelm slow receivers.

## Example Prompts

**Simple Echo Client:**

```
Connect to UDP at localhost:8080
Send datagram containing "HELLO"
When you receive a response, send it back
```

**DNS Query Client:**

```
Connect to UDP at 8.8.8.8:53
Send a DNS query for example.com (A record)
Parse the response and extract the IP address
```

**NTP Client:**

```
Connect to UDP at time.nist.gov:123
Send an NTP request
Parse the response and display the current time
```

**Multi-Target Client:**

```
Connect to UDP at localhost:8080
Send "PING" to localhost:8080
Send "HELLO" to localhost:9090
Wait for responses from both servers
```

## Testing Strategy

See `tests/client/udp/CLAUDE.md` for E2E test details.

**Test Server:** `nc -u -l localhost 8080` (netcat in UDP mode)

**Manual Test:**

```bash
# Terminal 1: Start UDP echo server
nc -u -l 8080

# Terminal 2: Start NetGet and open UDP client
./target/release/netget
> open_client udp localhost:8080 "Send HELLO and echo responses"

# Terminal 1: Type "RESPONSE" and press Enter
# LLM should receive and respond
```

## Future Enhancements

1. **Timeout Handling:** Scheduled tasks for request/response timeout
2. **Multicast Support:** Join/leave multicast groups
3. **Broadcast Support:** Enable SO_BROADCAST socket option
4. **Packet Loss Detection:** Track sequence numbers and detect gaps
5. **Rate Limiting:** Limit datagrams per second to prevent flooding
6. **Source Filtering:** Only accept datagrams from specific addresses
7. **Connection Simulation:** Stateful "pseudo-connection" tracking per source

## Implementation Notes

**Thread Safety:**

- `UdpSocket` wrapped in `Arc` for shared access
- Client data wrapped in `Arc<Mutex>` for state management
- Never hold Mutex during I/O operations (deadlock risk)

**Dual Logging:**

- All logs go through `tracing` macros (trace!, debug!, info!, warn!, error!)
- Important events also sent via `status_tx` for TUI display

**Error Handling:**

- Socket errors (ECONNREFUSED, ENETUNREACH) logged and client marked as Error status
- LLM errors reset client to Idle state and continue listening
- Invalid action parameters return errors without affecting socket state

## Command channel (injected actions)

The client registers a `command_support` command channel, so `AppState::send_to_client` — the
dashboard's `[ send ]` — can execute an action inside the running client. The channel is
registered **before** the `udp_connected` LLM call, which a `*` → manual routing rule can park
for minutes.

Commands are drained by their own registered task rather than a `select!` arm in the receive
loop: `recv_from` is cancellation-safe, but the loop body awaits an LLM call inline, so an arm
there would stall every injected command for the length of that call. The socket is an
`Arc<UdpSocket>` and `send_to` needs only `&self`, so no write-half mutex is involved.

Injected and LLM-produced actions share one code path, `UdpClient::apply_action_result`.

| Action | `ClientSendOutcome` |
|---|---|
| `send_udp_datagram` | `Sent { bytes_sent }` — the count `send_to` returned, so it is the real one |
| `change_target` | `Executed { "default target changed to …" }` — nothing reaches the wire |
| `wait_for_more` | `Executed` — the client is now Accumulating |
| `close_socket` | `Disconnected`; the command loop ends and the handle is dropped |
| anything else | `Rejected { error }` from the protocol's own executor |

The handle is removed on every exit: an injected `close_socket`, the receive loop's error exit,
and `remove_client`.
