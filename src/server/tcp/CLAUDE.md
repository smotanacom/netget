# TCP Protocol Implementation

## Overview

TCP server implementing raw TCP socket handling where the LLM has full control over the byte stream. This is the most
fundamental protocol in NetGet - the LLM can construct any TCP-based protocol on top of it (FTP, HTTP, SMTP, custom
protocols, etc.).

**Status**: Beta (Core Protocol)
**RFC**: RFC 793 (Transmission Control Protocol)

## Library Choices

- **tokio::net::TcpListener** - Async TCP server from Tokio runtime
- **tokio::io::{AsyncReadExt, AsyncWriteExt}** - Async I/O traits for reading/writing
- **Manual byte-level handling** - LLM receives raw bytes and constructs responses

**Rationale**: No high-level TCP library needed. The LLM directly controls the byte stream, making this the most
flexible protocol implementation.

## Architecture Decisions

### 1. Raw Byte Control

The LLM receives the exact bytes sent by the client and can respond with any byte sequence. This allows the LLM to:

- Implement any text protocol (FTP, SMTP, POP3, custom protocols)
- Handle binary protocols
- Mix text and binary data
- Create custom protocol parsers

### 2. Connection State Machine

Each connection has three states:

- **Idle**: Ready to process new data
- **Processing**: LLM is generating a response
- **Accumulating**: LLM requested `wait_for_more` to accumulate additional data

This prevents concurrent LLM calls for the same connection and ensures ordered processing.

### 3. Data Queueing

When data arrives while the LLM is processing:

1. Data is queued in `ConnectionData.queued_data`
2. After LLM response, queued data is merged and processed
3. Loop continues until all queued data is processed

`wait_for_more` puts the payload the model was just shown back at the **head** of that queue, so
the next event carries the fragment joined to whatever arrived after it. It used to drop the
fragment, which made the action's name a lie: a model asked to reassemble a message across two
reads could only do it by copying the first half into its memory. If bytes arrived *during* the
call that returned `wait_for_more`, they are the "more" it asked for, so the loop goes round
again immediately rather than parking them until a read that may never come — the peer has
typically sent its whole message and is waiting on us.

The queue is bounded by `MAX_QUEUED_BYTES` (8 MiB, `src/server/tcp/mod.rs`). Both paths that
grow it check the bound and close the connection (`decision=queue_overflow`, FIN) rather than
trimming, because a truncated payload the model cannot tell is truncated is worse than a
dropped connection. Without the bound a peer that streams for the length of one LLM call — a
few seconds — could grow NetGet's memory as fast as its link allows, pre-authentication.

### 4. Optional Banner Support

The `send_first` parameter allows servers to send a banner before receiving client data:

- Used for protocols like FTP, SMTP that send greeting on connection
- Triggers `TCP_CONNECTION_OPENED_EVENT` which the LLM handles
- Banner is sent immediately after connection acceptance

### 5. Stream Splitting

TcpStream is split into `(ReadHalf, WriteHalf)` using `tokio::io::split()`:

- **ReadHalf**: Owned by dedicated reader task
- **WriteHalf**: Wrapped in `Arc<Mutex<WriteHalf>>` and stored in connection map
- This allows concurrent reading and writing without cloning the stream

### 6. Dual Logging

All data operations use **dual logging**:

- **DEBUG**: Data summary with 100-char preview (for both text and binary)
- **TRACE**: Full payload (text as string, binary as hex)
- Both go to `netget.log` (via tracing) and TUI Status panel (via status_tx)

## LLM Integration

### Action-Based Response Model

The LLM responds to TCP events with actions:

**Events**:

- `tcp_connection_opened` - New connection accepted (only if `send_first=true`)
- `tcp_data_received` - Data received from client

**Available Actions**:

- `send_tcp_data` - Send raw bytes to client (`data` plus an explicit `encoding`, see
  [Data Format](#data-format))
- `close_this_connection` - Close the connection
- `wait_for_more` - Keep this payload and wait for the rest of the message
- Common actions: `show_message`, `update_instruction`, etc.

The one async (user-triggered) action is `close_connection`, which the dashboard's
`[ disconnect this peer ]` injects. Its `connection_id` is optional and ignored — the executor
is only ever reached with one connection in scope. `send_to_connection` and `list_connections`
used to be advertised alongside it and **could not work**: nothing ever populated
`TcpProtocol`'s own connection map, so `list_connections` always saw none, and the executor's
`Output` is written to whichever connection is being handled, so `send_to_connection` parsed and
then discarded its `connection_id`. Both are gone, the same removal `socket_file` made.

### Example LLM Response

```json
{
  "actions": [
    {
      "type": "send_tcp_data",
      "data": "220 Welcome to NetGet FTP Server\r\n",
      "encoding": "utf8"
    },
    {
      "type": "show_message",
      "message": "Sent FTP greeting"
    }
  ]
}
```

### Data Format

Outbound (`send_tcp_data`) uses an **explicit** `encoding` field next to
`data`. There is no heuristic sniffing: `"48656c6c6f"` is simultaneously valid text and valid
hex, so the sender must declare which it means.

| `encoding`          | Bytes written to the socket                                   |
| ------------------- | ------------------------------------------------------------- |
| omitted (default)   | `data.as_bytes()` — the string's characters, unchanged         |
| `"utf8"`            | same as omitted                                               |
| `"hex"`             | `hex::decode(data)` — `"48656c6c6f"` sends the 5 bytes `Hello` |
| anything else       | action fails with an error naming the valid values            |

Invalid hex (odd digit count, non-hex characters) returns an `anyhow` error rather than
panicking; the connection stays open. `encoding: "hex"` tolerates whitespace, `:` separators
and a leading `0x`. Defaulting to `utf8` keeps every pre-existing prompt, handler and test
working unchanged.

Inbound (`tcp_data_received`) carries the same pair of fields:

- `data` — the received bytes as text if **all** of them are ASCII graphic/whitespace,
  otherwise hex-encoded (`src/server/tcp/mod.rs`)
- `encoding` — `"utf8"` or `"hex"`, saying which of the two the `data` field is

Echoing is therefore symmetric: pass the event's `data` **and** its `encoding` straight into
`send_tcp_data` and the exact received bytes go back out.

## Connection Management

### Connection Lifecycle

1. **Accept**: `TcpListener::accept()` creates new connection
2. **Register**: Connection added to `ServerInstance` with `ProtocolConnectionInfo::Tcp`
3. **Split**: Stream split into ReadHalf and WriteHalf
4. **Track**: WriteHalf stored in `connections` HashMap with `ConnectionData`
5. **Handle**: Separate tasks for reading and LLM processing
6. **Close**: Connection removed from maps when client disconnects or LLM closes

**Step 4 happens synchronously in the accept loop, before any task is spawned.** It used to be
the first thing the banner task did, which raced the reader task spawned immediately after it:
`handle_data_with_actions` returns silently when the connection is not in the map, so a client
that wrote before the server accepted — the normal case, since `connect()` returns as soon as
the kernel completes the handshake — had its first payload dropped with no response, no error
and no log line. 8 of 64 clients in a burst lost their request that way. The banner task is now
spawned only when `send_first` is set, and `tests/connection_map_race_test.rs` pins the
behaviour. The same shape was fixed in `socket_file` (`1f3945ee`), `tls` and `ssh_agent`.

`handle_data_with_actions` releases the connection lock between its state check and the merge
step, so a connection can disappear underneath it (write-then-close clients do exactly that).
Both lookups return early instead of unwrapping; the merge step used to `unwrap()` and panicked
the task, which is silent — the server keeps reporting `Running`.

### Connection Data Structure

```rust
struct ConnectionData {
    state: ConnectionState,           // Idle/Processing/Accumulating
    queued_data: Vec<u8>,              // Data queued while processing
    memory: String,                    // Per-connection memory (unused currently)
    write_half: Arc<Mutex<WriteHalf>>, // For sending responses
}
```

### State Updates

- Connection state tracked in `ServerInstance.connections`
- Updates include: bytes sent/received, packets sent/received, last_activity
- UI automatically refreshes on state changes via `__UPDATE_UI__` message

## Known Limitations

### 1. No TLS Support

- Raw TCP only, no built-in TLS/SSL
- For HTTPS/FTPS, use the HTTP protocol with TLS or implement TLS manually

### 2. No Connection Timeouts

- Connections remain open indefinitely until client closes or LLM sends `close_connection`
- No idle timeout mechanism

### 3. No Backpressure Handling

- All received data is processed immediately
- Large bursts of data may overwhelm the LLM processing queue
- There is no flow control back to the peer; the only limit is `MAX_QUEUED_BYTES`, and reaching
  it closes the connection rather than slowing it

### 4. Memory Accumulation

- `wait_for_more` accumulates data in memory, bounded by `MAX_QUEUED_BYTES` (8 MiB per
  connection). Beyond that the connection is closed with `decision=queue_overflow`
- The bound is per connection, so total memory still scales with the number of peers

### 5. Half-Close Is a Failure Signal, Not a Feature

- The model cannot request a half-close: `close_this_connection` closes both directions
- The server half-closes (`write.shutdown()`) in exactly one case — the LLM call returned
  `Err` — for both the greeting (`send_first`) and the data path. Raw TCP has no error
  frame, so FIN is the only honest answer: the peer reads EOF immediately instead of
  blocking until its own timeout. Nothing derived from the error is ever written to the
  socket; the full error goes to the log and the status stream only
  (`decision=fail_closed_llm_error class=overloaded|unavailable`)
- The three outcomes are distinguishable in the log: `decision=model_close` (the model hung
  up), `decision=model_no_actions` (the model answered with no bytes — legitimate on raw
  TCP, connection stays open), `decision=fail_closed_llm_error` (the backend failed)

## Example Prompts

### FTP Server

```
listen on port 21 via ftp
When a client connects, send "220 NetGet FTP Server\r\n"
Handle USER command: respond with "331 Password required\r\n"
Handle PASS command: respond with "230 Login successful\r\n"
Handle PWD command: respond with "257 \"/home/user\"\r\n"
Handle QUIT command: respond with "221 Goodbye\r\n" and close connection
```

### Echo Server

```
listen on port 7 via tcp
When you receive any data, echo it back with "ACK: " prefix
```

### Binary Protocol

```
listen on port 9000 via tcp
When you receive a 4-byte big-endian integer, respond with the integer + 1
Reply with send_tcp_data using "encoding": "hex" so the digits are decoded into bytes
```

### Stateful Protocol

```
listen on port 8080 via tcp
Wait for command: HELLO, START, or STOP
After HELLO, send "READY\r\n"
After START, send "RUNNING\r\n"
After STOP, send "STOPPED\r\n" and close connection
```

## Performance Characteristics

### Latency

- One LLM call per received data chunk (unless using `wait_for_more`)
- Typical latency: 2-5 seconds per request with qwen3-coder:30b

### Throughput

- Limited by LLM response time
- Concurrent connections processed in parallel (each on separate tokio task)
- Queue mechanism prevents data loss but doesn't improve throughput

### Concurrency

- Unlimited concurrent connections (bounded by system resources)
- Each connection has independent state and processing
- Ollama lock serializes LLM API calls across all connections

## References

- [RFC 793: Transmission Control Protocol](https://datatracker.ietf.org/doc/html/rfc793)
- [Tokio TcpListener](https://docs.rs/tokio/latest/tokio/net/struct.TcpListener.html)
- [Tokio AsyncReadExt](https://docs.rs/tokio/latest/tokio/io/trait.AsyncReadExt.html)
- [Tokio AsyncWriteExt](https://docs.rs/tokio/latest/tokio/io/trait.AsyncWriteExt.html)
