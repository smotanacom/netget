# IGMP Client Implementation

## Overview

The IGMP (Internet Group Management Protocol) client enables NetGet to join and leave multicast groups, receive
multicast data, and send multicast packets. This implementation uses socket options for multicast group management,
which doesn't require root privileges for basic operations.

## Library Choices

### Primary Approach: socket options on the client's own tokio socket

- **Library**: `tokio::net::UdpSocket`'s `join_multicast_v4` / `leave_multicast_v4`, which are
  the same `IP_ADD_MEMBERSHIP` / `IP_DROP_MEMBERSHIP` options `socket2` exposes. They are called
  on the socket the receive loop polls — not on a socket created for the call, which is what the
  first version did and why the membership died the moment the match arm ended.
- **Purpose**: Multicast group join/leave using `IP_ADD_MEMBERSHIP` and `IP_DROP_MEMBERSHIP`
- **Privileges**: No root required for receiving multicast
- **Pros**:
    - Simple and portable
    - Kernel handles IGMP protocol messages automatically
    - Works on all platforms (Linux, macOS, Windows)
    - No raw socket privileges needed
- **Cons**:
    - Cannot manually craft IGMP packets
    - Relies on kernel's IGMP implementation

### Data Transport: tokio::net::UdpSocket

- **Library**: `tokio::net::UdpSocket` (tokio standard library)
- **Purpose**: Receiving multicast UDP datagrams and sending multicast data
- **Integration**: Seamless async I/O with tokio runtime

## Architecture

### Connection Model

The IGMP client is **connectionless** but maintains **active listening state**:

1. **Bind Phase**: Create UDP socket bound to `0.0.0.0:PORT` (or user-specified)
2. **Active State**: Client remains active to receive multicast data
3. **Group Management**: Join/leave multicast groups dynamically via LLM actions
4. **Reception Loop**: Async loop receives multicast datagrams from joined groups

### No state machine, deliberately

Most protocols here carry an Idle → Processing → Accumulating machine, and this client used to
carry a copy of it. It could never fire. One task does both the `recv_from` and the `await` on
the LLM call, so `Processing` was unobservable, the queue it guarded was only ever pushed to
from the unreachable arm, and the Idle arm ended by **clearing** that queue — so had it ever
worked it would have silently dropped the datagrams it collected. It is gone. Datagrams that
arrive during an LLM round-trip wait in the kernel socket buffer, which is where they belong.

Do not reintroduce one without a second reader to guard against.

### Multicast Group Tracking

The client tracks joined multicast groups in `IgmpClientData::joined_groups` (HashSet<Ipv4Addr>). This allows:

- Preventing duplicate joins
- Tracking active memberships
- Clean leave on client shutdown (future enhancement)

## LLM Integration

### Events

1. **igmp_connected** - Triggered when client binds and is ready
    - Parameters: `local_addr` (socket bind address)

2. **igmp_data_received** - Triggered on multicast datagram reception
    - Parameters:
        - `data_hex`: hex of the datagram, **capped at the first 2048 bytes**
          (`MAX_EVENT_PAYLOAD_BYTES`). A UDP datagram can be 64 KiB, which is 128 KiB of hex —
          enough to fill a model's context with one frame, and to do it again for the next.
        - `data_length`: the **full** size in bytes, whether or not `data_hex` was cut
        - `data_truncated`: true when `data_hex` is a prefix. The model is told what it is not
          being shown rather than quietly handed one.
        - `source_addr`: Sender's IP:port

### Actions

#### Async Actions (User-triggered)

1. **join_multicast_group**
    - Joins an IPv4 multicast group
    - Parameters:
        - `multicast_addr`: Multicast IP (e.g., `239.1.2.3`)
        - `interface_addr`: Local interface (default: `0.0.0.0` for any)
    - Effect: Kernel sends IGMP Membership Report
    - Example:
      ```json
      {
        "type": "join_multicast_group",
        "multicast_addr": "239.1.2.3",
        "interface_addr": "0.0.0.0"
      }
      ```

2. **leave_multicast_group**
    - Leaves an IPv4 multicast group
    - Parameters: Same as join
    - Effect: Kernel sends IGMP Leave Group message
    - Example:
      ```json
      {
        "type": "leave_multicast_group",
        "multicast_addr": "239.1.2.3"
      }
      ```

3. **send_multicast**
    - Sends data to a multicast group
    - Parameters:
        - `multicast_addr`: Destination multicast IP — parsed as an `Ipv4Addr` by
          `execute_action`, not at send time, so a bad value names the field the model got wrong
          instead of failing on a synthesised `"addr:port"` string
        - `port`: Destination port — range-checked to `u16` for the same reason
        - `data_hex`: Hex-encoded payload. The field is documented as hex and the executor
          **decodes** it (`hex::decode`), so what reaches the wire is the bytes, never the ASCII
          of the hex string. The e2e test used to mock this as `"data"`, which is not the
          declared field: `execute_action` refused it, no datagram was sent, and the test passed
          anyway. See `tests/client/igmp/e2e_test.rs`.
    - Example:
      ```json
      {
        "type": "send_multicast",
        "multicast_addr": "239.1.2.3",
        "port": 5000,
        "data_hex": "48656c6c6f"
      }
      ```

#### Sync Actions (Response to events)

1. **wait_for_more** - Accumulate more multicast data before responding

## Implementation Details

### Multicast Join/Leave

`UdpSocket::join_multicast_v4()` / `leave_multicast_v4()` on the client's own socket:

```rust
// On the client's OWN receive socket - see the bug note at the foot of this file.
socket.join_multicast_v4(multicast_ip, interface_ip)?;  // Kernel sends IGMP report
socket.leave_multicast_v4(multicast_ip, interface_ip)?; // Kernel sends IGMP leave
```

**Important**: These operations trigger the kernel to send IGMP protocol messages (Membership Report, Leave Group). The
client doesn't construct raw IGMP packets — which is also why an injected join reports
`Executed`, never `Sent`: NetGet writes no bytes of its own.

A repeated join is answered from `joined_groups` rather than passed to the kernel, so it reads
`Executed { detail: "already a member of …" }` instead of failing with `EADDRINUSE`.

### Multicast Reception

The UDP socket automatically receives multicast datagrams once joined to a group:

```rust
let socket = UdpSocket::bind("0.0.0.0:PORT").await?;
// After join_multicast_group, socket receives multicast data
let (n, peer_addr) = socket.recv_from(&mut buffer).await?;
```

### Multicast Sending

Sending to a multicast group uses standard UDP send:

```rust
socket.send_to(&data, "239.1.2.3:5000").await?;
```

**TTL Consideration**: Default TTL=1 (link-local). For broader multicast, set TTL explicitly (future enhancement).

## Limitations

### 1. IPv4 Only

- Current implementation supports only IPv4 multicast (224.0.0.0/4)
- IPv6 multicast (ff00::/8) requires different socket options (`IPV6_ADD_MEMBERSHIP`)

### 2. No Raw IGMP Packet Construction

- Cannot manually craft IGMP packets (requires raw sockets + root)
- Cannot send custom IGMP queries or reports
- Relies entirely on kernel's IGMP implementation

### 3. Single Interface Binding

- Socket binds to `0.0.0.0` (all interfaces) by default
- Cannot explicitly select interface for joins without additional socket configuration

### 4. No IGMP Version Control

- Kernel chooses IGMP version (IGMPv1/v2/v3) based on router behavior
- Client cannot force specific IGMP version

### 5. No Group-Source Filtering (IGMPv3)

- Cannot use source-specific multicast (SSM)
- Would require `IP_ADD_SOURCE_MEMBERSHIP` socket option (future enhancement)

## Testing Considerations

### Local Testing

Multicast works on localhost but has limitations:

- **Loopback Multicast**: Enabled by default on most systems
- **Same Machine**: Can test sender/receiver on same host
- **Firewall**: Ensure multicast traffic (224.0.0.0/4) is not blocked

### Network Testing

For real multicast testing:

- **Multicast-Capable Network**: Router must support IGMP
- **IGMP Querier**: At least one IGMP querier on subnet
- **TTL**: Ensure TTL > 1 for multi-hop multicast

### Common Multicast Groups

- **224.0.0.1**: All hosts on subnet
- **224.0.0.2**: All routers on subnet
- **239.0.0.0/8**: Administratively scoped (organization-local)

## Example Prompts

1. **Join Group and Listen**:
   ```
   Join multicast group 239.1.2.3 and wait for data
   ```

2. **Send Hello Message**:
   ```
   Send "Hello Multicast" to group 239.1.2.3 port 5000
   ```

3. **Leave Group**:
   ```
   Leave multicast group 239.1.2.3
   ```

4. **Monitor Multiple Groups**:
   ```
   Join groups 239.1.1.1 and 239.1.2.2, log all received data
   ```

## Future Enhancements

1. **IPv6 Multicast Support**: Add `IPV6_ADD_MEMBERSHIP`
2. **TTL Control**: Allow LLM to set multicast TTL
3. **Source-Specific Multicast (SSM)**: Use `IP_ADD_SOURCE_MEMBERSHIP` for IGMPv3
4. **Interface Selection**: Bind to specific network interface
5. **Multicast Loopback Control**: Enable/disable receiving own sent multicast
6. **Raw IGMP Mode**: Optional raw socket mode for crafting IGMP packets (requires root)

## Comparison to Server Implementation

NetGet also has an IGMP **server** (`src/server/igmp/`), which differs:

| Feature          | Client                                | Server                                |
|------------------|---------------------------------------|---------------------------------------|
| **Purpose**      | Join/leave groups, receive multicast  | Respond to IGMP queries (router-like) |
| **Socket Type**  | UDP socket (port 0 or user-specified) | Raw IP socket (protocol 2)            |
| **Privileges**   | No root required                      | Requires root/CAP_NET_RAW             |
| **IGMP Packets** | Kernel handles                        | Manual construction                   |
| **Use Case**     | Multicast data consumer               | IGMP protocol testing                 |

## Security Considerations

1. **Multicast Amplification**: Be cautious sending to large multicast groups
2. **Firewall Rules**: Multicast may be blocked by firewalls
3. **Local Network Only**: Most multicast is scoped to local networks (TTL=1)
4. **No Authentication**: Multicast has no built-in authentication mechanism

## References

- RFC 1112: Host Extensions for IP Multicasting (IGMPv1)
- RFC 2236: Internet Group Management Protocol, Version 2 (IGMPv2)
- RFC 3376: Internet Group Management Protocol, Version 3 (IGMPv3)
- RFC 4607: Source-Specific Multicast for IP

## Injected commands (the dashboard's `[ send ]`)

The client registers a command channel (`command_support::register_command_channel`) and spawns
the task that drains it **before** the connected-event LLM call, which is awaited inline in
`connect_with_llm_actions` and which a manual `*` routing rule parks. Commands are drained by
their own registered task rather than a `tokio::select!` arm: `recv_from` is cancellation-safe,
but the receive loop awaits `call_llm_for_client` inline, so a `select!` arm there would stall
for a whole LLM round-trip.

Injected actions go through `IgmpClient::apply_action`, the same function the connected-event
path and the receive loop use. Outcomes:

| Injected action | `ClientSendOutcome` |
|---|---|
| `send_multicast` | `Sent { bytes_sent }` — the byte count `send_to` returned |
| `join_multicast_group` / `leave_multicast_group` | `Executed { detail }` — **not** `Sent`. NetGet writes no bytes itself; `IP_ADD_MEMBERSHIP` makes the *kernel* emit the IGMP membership report, and the detail says so |
| `wait_for_more` | `Executed { detail: "wait_for_more" }` |
| unknown action, malformed parameters, **and `disconnect`** | `Rejected { error }` — IGMP's vocabulary has no `disconnect` verb (see `get_async_actions`); ending the client is `remove_client`, not a wire action |

Two bugs were fixed while wiring this up, because both would have made the reported outcome a
lie:

1. **Group joins were applied to a throwaway socket.** `join_multicast_group` created a fresh
   `socket2::Socket`, joined the group on it, and dropped it at the end of the match arm — so
   the membership died immediately and the `UdpSocket` the receive loop polls never joined
   anything. The client logged "Joined multicast group X" and received no multicast. Joins now
   run on the client's own socket via `UdpSocket::join_multicast_v4`, and a duplicate join is
   reported as `Executed { detail: "already a member of …" }` instead of failing with
   `EADDRINUSE`.
2. **The connected event's actions were discarded.** The initial `call_llm_for_client` was
   `let _ = …`, so a model answering `igmp_client_connected` with a join was ignored. Its
   actions now go through the same `apply_action`.

Covered by `tests/client/igmp/command_channel_test.rs`. That test sends to a loopback UDP
socket rather than a real group: `send_multicast` is a plain `send_to` on the client's own
socket, so this exercises the whole injected path, while a real group would make the test
depend on the host having a multicast-capable interface.
