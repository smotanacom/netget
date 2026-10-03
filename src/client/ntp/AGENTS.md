# NTP Client Implementation

## Overview

UDP-based NTP (Network Time Protocol) client that queries time servers and interprets responses. The LLM can trigger
time queries and analyze timestamps, stratum levels, and precision.

## Architecture

### Protocol Stack

- **Transport:** UDP (port 123 by default)
- **Packet Size:** Fixed 48 bytes (NTP v3/v4 format)
- **Request-Response:** Single query per connection

### Connection Model

- **Connectionless:** UDP-based, no persistent connection
- **Timeout:** 5 second timeout for responses
- **Single Query:** Each client performs one query and terminates

## Library Choices

### No External NTP Library

Manual implementation chosen because:

1. **Simplicity:** NTP request packet is only 48 bytes with fixed structure
2. **Control:** Full control over packet construction and parsing
3. **Dependencies:** Avoids external crates for simple protocol
4. **Standard:** Uses Tokio's `UdpSocket` for UDP communication

### Packet Structure

#### NTP Request (48 bytes)

```
Byte 0: LI | VN | Mode
  LI (2 bits): Leap Indicator = 0
  VN (3 bits): Version = 3
  Mode (3 bits): Client = 3
  => 0x1b = 0b00011011

Bytes 1-39: Zeros (unused in client request)
Bytes 40-47: Transmit Timestamp
  Bytes 40-43: Seconds since NTP epoch (Jan 1, 1900)
  Bytes 44-47: Fraction of seconds
```

#### NTP Response (48 bytes)

```
Byte 0: LI | VN | Mode
Byte 1: Stratum (0-15, clock distance from reference)
Byte 2: Poll interval
Byte 3: Precision (log2 seconds)
Bytes 4-7: Root delay
Bytes 8-11: Root dispersion
Bytes 12-15: Reference ID
Bytes 16-23: Reference timestamp
Bytes 24-31: Origin timestamp (client's transmit time)
Bytes 32-39: Receive timestamp (server received request)
Bytes 40-47: Transmit timestamp (server sent response)
```

### Timestamp Format

- **NTP Epoch:** January 1, 1900 00:00:00 UTC
- **Unix Epoch:** January 1, 1970 00:00:00 UTC
- **Conversion:** Unix timestamp = NTP timestamp - 2,208,988,800

## LLM Integration

### Connection Flow

1. **Client Connect:** Bind UDP socket to local port
2. **Initial LLM Call:** Get instruction, usually triggers `query_time` action
3. **Send NTP Request:** Build and send 48-byte packet
4. **Wait for Response:** 5 second timeout
5. **Parse Response:** Extract timestamps, stratum, precision
6. **LLM Analysis:** Call LLM with response event containing parsed data
7. **Disconnect:** Client terminates after single query

### Events

#### `ntp_connected`

Triggered when client is ready to query servers.

**Parameters:**

- `remote_addr`: NTP server address

#### `ntp_response_received`

Triggered when NTP response is received.

**Parameters:**

- `origin_timestamp`: Client's transmit time (Unix epoch)
- `receive_timestamp`: Server received request (Unix epoch)
- `transmit_timestamp`: Server sent response (Unix epoch)
- `stratum`: Server stratum level (0-15)
    - 0 = unspecified
    - 1 = primary reference (GPS, atomic clock)
    - 2-15 = secondary references (distance from stratum 1)
- `precision`: Server clock precision (log2 seconds)
    - -20 = ~1 microsecond
    - -10 = ~1 millisecond
    - -1 = ~0.5 seconds

### Actions

#### User Actions (`get_async_actions`)

1. **`query_time`**
    - **Description:** Query NTP server for current time
    - **Parameters:** None
    - **Result:** Sends NTP request packet

2. **`disconnect`**
    - **Description:** Close NTP client
    - **Parameters:** None
    - **Result:** Terminates client

#### Response Actions (`get_sync_actions`)

1. **`analyze_response`**
    - **Description:** Analyze NTP response (informational)
    - **Parameters:** None
    - **Result:** No-op (LLM understanding only)

## Example Usage

### Query Time Server

```
User: "Query time.google.com:123 and calculate time offset"

LLM Actions:
1. query_time -> Sends NTP request
2. (Receives response with timestamps)
3. analyze_response -> Calculates offset and displays result
```

### Stratum Analysis

```
User: "Check pool.ntp.org:123 stratum level"

LLM Actions:
1. query_time
2. (Response: stratum=2, precision=-20)
3. analyze_response -> "Stratum 2 server (secondary reference), microsecond precision"
```

## Limitations

### Single Query Model

- **One Query Per Client:** Each client performs one query and terminates
- **No Continuous Sync:** Not a daemon for continuous time synchronization
- **No Clock Adjustment:** Does not adjust system clock (read-only queries)

### Timeout Handling

- **5 Second Timeout:** Fixed timeout for UDP response
- **No Retry:** Single attempt, no automatic retry on failure
- **LLM Not Called on Timeout:** Timeout errors logged but don't trigger LLM

### Protocol Simplification

- **NTP v3 Format:** Uses simplified v3 client format (compatible with v4)
- **No Authentication:** Does not support NTP authentication extensions
- **No Broadcast/Multicast:** Unicast queries only
- **Fraction Field Zeroed:** Transmit timestamp fraction always 0 (second precision)

### Clock Offset Calculation

- **LLM Responsibility:** LLM must calculate offset from timestamps
- **No Automatic Calculation:** Client provides raw timestamps only
- **Formula:** offset = ((T2 - T1) + (T3 - T4)) / 2
    - T1 = origin (client transmit)
    - T2 = receive (server receive)
    - T3 = transmit (server transmit)
    - T4 = destination (client receive, not included in response)

## Security Considerations

### Public NTP Servers

- **No Authentication:** Responses are unauthenticated
- **Spoofing Risk:** UDP responses can be spoofed
- **Trust Model:** Trust NTP server operator

### Localhost Only

- **Default Bind:** Binds to 0.0.0.0:0 (any local address, random port)
- **No External Listen:** Does not listen on fixed ports
- **Ephemeral Ports:** Uses OS-assigned ephemeral source port

## Testing Strategy

### E2E Testing

- **Public NTP Servers:** Test against pool.ntp.org, time.google.com
- **Stratum Verification:** Check stratum is reasonable (1-15)
- **Timestamp Validation:** Verify timestamps are recent (within 1 hour)
- **LLM Call Budget:** < 3 LLM calls per test (1 initial, 1 response)

### Unit Testing

- **Packet Construction:** Verify request packet format (0x1b header, timestamps)
- **Packet Parsing:** Verify response parsing (timestamps, stratum, precision)
- **Timestamp Conversion:** Verify NTP-to-Unix conversion (subtract 2,208,988,800)

## Known Issues

1. **Single Query Only:** Client terminates after one query (by design)
2. **No Retry Logic:** Network failures require new client instance
3. **Fraction Field Ignored:** Request transmit timestamp has zero fraction
4. **Destination Timestamp Missing:** LLM cannot calculate round-trip delay without T4

## Future Enhancements

### Multi-Query Support

- Allow LLM to trigger multiple queries per client
- Add `wait_for_more` action to keep client alive

### Statistics

- Track multiple queries to same server
- Calculate average offset and jitter
- Detect clock drift

### Authentication

- Support NTP authentication (MAC field in extension)
- Validate server responses with shared keys

### Precision

- Use high-resolution timers for fraction field
- Calculate round-trip delay and local clock offset
- Implement NTP algorithms (clock filter, selection, clustering)

## Command channel (injected actions)

The client registers a `command_support` command channel, so `AppState::send_to_client` — the
dashboard's `[ send ]` — can execute an action inside the running client. The channel is
registered **before** the initial LLM call, which a `*` → manual routing rule can park for
minutes.

**This is also the client's multi-query path.** The connect-time task still marks the client
`Disconnected` after its single query, but the UDP socket stays bound and the command channel
stays registered, so an injected `query_time` runs another exchange. That is the closest thing
this client has to the "multi-query support" listed under Future Enhancements.

| Action | `ClientSendOutcome` |
|---|---|
| `query_time` | `Sent { bytes_sent: 48 }` — the count `send_to` returned for the request datagram |
| `analyze_response` | `Executed` — interpretation only, nothing reaches the wire |
| `disconnect` | `Disconnected`; the command loop ends and the handle is dropped |
| anything else | `Rejected { error }` |

Two details worth knowing:

- The caller is answered **as soon as the request is on the wire**. The reply is then received
  and handed to the model as `ntp_response_received` *after* the reply is sent, so a manual
  rule parked on that event cannot hold `[ send ]` open for its whole timeout. The cost is that
  the command loop is busy until the reply arrives or its 5-second timeout expires, so a
  command injected immediately after a `query_time` queues behind it.
- The connect-time query and an injected query share a `Mutex` held across the receive, so they
  cannot steal each other's datagram.
