# BOOTP Client Implementation

## Overview

BOOTP (Bootstrap Protocol, RFC 951) client implementation for diskless workstation boot discovery, PXE boot testing, and
TFTP server location. BOOTP is a UDP-based protocol that predates DHCP and is primarily used for network boot scenarios.

## Library Choice

**Primary Library:** `dhcproto` v0.12

- Pure Rust BOOTP/DHCP packet encoding and decoding
- BOOTP is a subset of DHCP (DHCP packets without options)
- Mature, well-maintained, used by many Rust networking projects
- Supports both BOOTP and DHCP message formats

**Why dhcproto:**

- BOOTP uses the same packet format as DHCP (RFC 951 evolved into RFC 2131)
- No need for separate BOOTP library - DHCP without options = BOOTP
- Excellent packet parsing and validation
- Type-safe message construction

## Architecture

### Connection Model

BOOTP is connectionless (UDP-based):

1. Client binds to UDP port 68 (BOOTP client port, requires elevated privileges)
2. Fallback: If port 68 binding fails, bind to random port
3. Enable broadcast socket option for broadcast replies
4. Send BOOTP request to server (port 67) or broadcast (255.255.255.255:67)
5. Receive BOOTP reply with IP assignment and boot information
6. LLM decides whether to send additional requests or terminate

**Key Differences from TCP Clients:**

- No persistent connection (UDP is stateless)
- Each request/reply is independent
- Binding to port 68 may require root/elevated privileges
- Broadcast support required for standard BOOTP operation

### BOOTP Message Format

BOOTP uses a fixed 300-byte packet structure:

```
+---------------------+
| op (1 byte)         | BOOTREQUEST (1) or BOOTREPLY (2)
| htype (1 byte)      | Hardware address type (1 = Ethernet)
| hlen (1 byte)       | Hardware address length (6 for MAC)
| hops (1 byte)       | Hop count
| xid (4 bytes)       | Transaction ID (random)
| secs (2 bytes)      | Seconds elapsed
| flags (2 bytes)     | Flags (0x8000 = broadcast)
| ciaddr (4 bytes)    | Client IP address (if known)
| yiaddr (4 bytes)    | Your IP address (assigned by server)
| siaddr (4 bytes)    | Server IP address (TFTP server)
| giaddr (4 bytes)    | Gateway IP address (relay agent)
| chaddr (16 bytes)   | Client hardware address (MAC + padding)
| sname (64 bytes)    | Server host name (optional)
| file (128 bytes)    | Boot file name (e.g., "pxelinux.0")
| vend (64 bytes)     | Vendor-specific area (unused in pure BOOTP)
+---------------------+
```

**BOOTP vs DHCP:**

- BOOTP: Fixed format, no options field
- DHCP: Same format + variable-length options (DHCP is backward compatible with BOOTP)
- `dhcproto` handles both by treating BOOTP as DHCP with empty options

## State Management

### Connection State Machine

BOOTP client uses the same state machine as other clients:

- **Idle**: Ready to process incoming replies
- **Processing**: LLM is analyzing a reply and generating actions
- **Accumulating**: Additional replies received during processing are queued

**State Flow:**

```
Idle → [Reply received] → Processing → [LLM complete] → Idle
                              ↓
                        [More replies]
                              ↓
                         Accumulating → [LLM complete] → Idle
```

### Per-Client Data

Each BOOTP client maintains:

```rust
struct ClientData {
    state: ConnectionState,
    queued_replies: Vec<BootpReply>,  // Queued during processing
    memory: String,                    // LLM conversation memory
}
```

### Parsed Reply Data

BOOTP replies are parsed into structured data for LLM consumption:

```rust
struct BootpReply {
    assigned_ip: Ipv4Addr,     // yiaddr - IP address assigned
    server_ip: Ipv4Addr,       // siaddr - TFTP/boot server
    gateway_ip: Ipv4Addr,      // giaddr - Gateway/relay agent
    boot_filename: String,     // file - Boot file (e.g., "pxelinux.0")
}
```

## LLM Integration

### Events

**1. bootp_connected**

- Triggered when UDP socket is bound and ready
- Parameters:
    - `server_addr` (string): BOOTP server address

**2. bootp_reply_received**

- Triggered when BOOTP reply is received from server
- Parameters:
    - `assigned_ip` (string): IP address assigned by server (yiaddr field)
    - `server_ip` (string): Boot/TFTP server IP (siaddr field)
    - `boot_filename` (string): Boot file name (file field, e.g., "pxelinux.0")
    - `gateway_ip` (string): Gateway/relay agent IP (giaddr field)

### Actions

**Async Actions (User-Triggered):**

1. **send_bootp_request**
    - Send BOOTP request to discover boot server
    - Parameters:
        - `client_mac` (string, required): Client MAC address (format: `00:11:22:33:44:55`)
        - `broadcast` (boolean, optional): Use broadcast (true) or unicast (false), default: true
    - Example:
      ```json
      {
        "type": "send_bootp_request",
        "client_mac": "00:11:22:33:44:55",
        "broadcast": true
      }
      ```

2. **disconnect**
    - Close BOOTP client (for consistency, though UDP is connectionless)
    - Parameters: none
    - Example:
      ```json
      {
        "type": "disconnect"
      }
      ```

**Sync Actions (Network Event Response):**

1. **send_bootp_request**
    - Send another BOOTP request in response to a reply
    - Same parameters as async version

2. **wait_for_more**
    - Wait for more BOOTP replies before responding
    - Parameters: none

### LLM Flow

**Initial Connection:**

1. User: `open_client bootp 192.168.1.1:67 "Request IP for MAC 00:11:22:33:44:55"`
2. System: Bind UDP socket, send `bootp_connected` event
3. LLM: Receives connected event, returns `send_bootp_request` action
4. System: Constructs and sends BOOTP request

**Reply Processing:**

1. System: Receives BOOTP reply, parses into structured data
2. System: Checks state machine (Idle → Processing)
3. System: Sends `bootp_reply_received` event to LLM
4. LLM: Analyzes reply (IP, server, boot file), returns action or `wait_for_more`
5. System: Executes action, returns to Idle state

**Example LLM Reasoning:**

```
Event: bootp_reply_received
  assigned_ip: 192.168.1.100
  server_ip: 192.168.1.1
  boot_filename: pxelinux.0
  gateway_ip: 0.0.0.0

LLM Analysis:
- Server assigned IP 192.168.1.100 to client
- Boot server is 192.168.1.1
- Boot file is "pxelinux.0" (PXE Linux boot loader)
- No gateway/relay agent (direct server)

Action: disconnect (information retrieved successfully)
```

## Use Cases

### 1. PXE Boot Testing

Test PXE boot server configuration:

```
User: "Connect to BOOTP server 192.168.1.1:67 and request boot info for MAC 52:54:00:12:34:56"

LLM Actions:
1. send_bootp_request(client_mac="52:54:00:12:34:56", broadcast=true)
2. [Receive reply with IP and boot file]
3. Analyze boot filename (e.g., "pxelinux.0") and server IP
4. disconnect
```

### 2. Diskless Workstation Simulation

Simulate diskless workstation boot:

```
User: "Simulate diskless workstation boot discovery"

LLM Actions:
1. send_bootp_request(client_mac="00:11:22:33:44:55", broadcast=true)
2. [Receive reply with yiaddr=192.168.1.100, siaddr=192.168.1.1, file="tftp-boot.img"]
3. Log: "Boot server: 192.168.1.1, Assigned IP: 192.168.1.100, Boot file: tftp-boot.img"
4. (LLM could optionally continue by opening a TFTP client to download the boot file)
```

### 3. BOOTP Server Discovery

Find BOOTP servers on network:

```
User: "Discover BOOTP servers on network"

LLM Actions:
1. send_bootp_request(client_mac="00:11:22:33:44:55", broadcast=true)
2. wait_for_more (multiple servers may respond)
3. [Receive replies from multiple servers]
4. Analyze all replies and report server IPs
```

### 4. Legacy Network Testing

Test compatibility with legacy BOOTP infrastructure:

```
User: "Test legacy BOOTP compatibility"

LLM Actions:
1. send_bootp_request with various MAC addresses
2. Analyze reply format (pure BOOTP vs DHCP with options)
3. Verify server behavior matches BOOTP spec (RFC 951)
```

## Implementation Details

### Port Binding

BOOTP clients traditionally bind to port 68:

```rust
match UdpSocket::bind("0.0.0.0:68").await {
    Ok(s) => s,
    Err(e) => {
        warn!("Failed to bind to port 68 (requires privileges): {}. Using random port.", e);
        UdpSocket::bind("0.0.0.0:0").await?
    }
}
```

**Why port 68 is preferred:**

- BOOTP servers send replies to port 68 by default
- Some servers may reject replies to non-standard ports
- PXE boot requires port 68 for proper operation

**Fallback to random port:**

- Allows testing without elevated privileges
- Works with servers that support unicast replies to any port
- May not work with all BOOTP/DHCP server implementations

### Broadcast Support

BOOTP requires broadcast capability:

```rust
socket.set_broadcast(true)?;
```

Broadcast is used for:

- Sending requests when server address is unknown (255.255.255.255:67)
- Receiving broadcast replies (flag 0x8000 in BOOTP request)

### MAC Address Parsing

LLM provides MAC addresses as strings (e.g., `00:11:22:33:44:55`):

```rust
fn parse_mac(mac_str: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = mac_str.split(':').collect();
    if parts.len() != 6 {
        return Err(anyhow!("Invalid MAC address format"));
    }

    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(part, 16)?;
    }
    Ok(mac)
}
```

### Transaction ID

Each BOOTP request generates a random transaction ID (xid):

```rust
msg.set_xid(rand::random::<u32>());
```

This allows:

- Matching replies to requests
- Preventing reply spoofing (though BOOTP has minimal security)
- Supporting concurrent requests (though typically only one is needed)

## Limitations

### 1. Privilege Requirements

- Binding to port 68 requires elevated privileges (root/administrator)
- Fallback to random port works but may not be compatible with all servers

### 2. No Authentication

- BOOTP has no authentication mechanism
- Replies can be spoofed (rogue BOOTP servers)
- Use in trusted networks only

### 3. Limited Information

- BOOTP provides only basic boot information (IP, server, file name)
- No subnet mask, DNS, NTP, or other options (use DHCP for that)
- Server name field (sname) is often unused

### 4. Broadcast Dependency

- Relies on broadcast for discovery
- May not work across routed networks without BOOTP relay agents
- Some network configurations block broadcast traffic

### 5. No Lease Management

- BOOTP does not manage IP address leases
- No concept of lease time, renewal, or release
- Static IP assignment is assumed

## Comparison: BOOTP vs DHCP Client

| Feature              | BOOTP Client           | DHCP Client                             |
|----------------------|------------------------|-----------------------------------------|
| **Protocol**         | BOOTP (RFC 951)        | DHCP (RFC 2131)                         |
| **Options**          | None (fixed format)    | Many (subnet, DNS, NTP, etc.)           |
| **Use Case**         | Diskless boot, PXE     | General IP configuration                |
| **Lease Management** | No                     | Yes (lease time, renewal)               |
| **State Machine**    | Simple (request/reply) | Complex (DISCOVER, OFFER, REQUEST, ACK) |
| **Authentication**   | None                   | Optional (Option 82, etc.)              |
| **Port**             | 68 (client)            | 68 (client)                             |
| **Library**          | dhcproto (no options)  | dhcproto (with options)                 |

**When to use BOOTP client:**

- Testing PXE boot servers
- Simulating diskless workstations
- Legacy network compatibility
- Boot file discovery

**When to use DHCP client:**

- Full IP configuration (subnet, DNS, gateway)
- Lease management and renewal
- Modern network environments
- Option negotiation (e.g., PXE vendor options)

## Testing Strategy

See `tests/client/bootp/CLAUDE.md` for detailed testing approach.

**Key Testing Scenarios:**

1. Broadcast BOOTP request with synthetic MAC
2. Parse BOOTP reply (IP, server, boot file)
3. Verify LLM interprets reply correctly
4. Test multiple replies (server discovery)
5. Privilege fallback (random port if 68 unavailable)

**Test Server Options:**

- `dnsmasq` (simple BOOTP/DHCP server)
- `isc-dhcp-server` (full-featured)
- Custom BOOTP responder (netget server in future)

## Future Enhancements

### 1. DHCP Options Support

- Extend to support DHCP options (subnet mask, DNS, gateway)
- Upgrade to full DHCP client (DISCOVER, OFFER, REQUEST, ACK state machine)

### 2. TFTP Integration

- After receiving BOOTP reply, automatically connect TFTP client to download boot file
- Simulate complete PXE boot sequence

### 3. Relay Agent Support

- Support BOOTP relay agents (giaddr field)
- Test relay agent behavior in routed networks

### 4. Multiple Request Strategies

- Retry logic with exponential backoff
- Broadcast then unicast fallback
- Multiple MAC address probing

### 5. Reply Validation

- Verify transaction ID matches request
- Detect rogue BOOTP servers (multiple conflicting replies)
- Sanity check assigned IP address

## References

- **RFC 951**: Bootstrap Protocol (BOOTP) - https://www.rfc-editor.org/rfc/rfc951
- **RFC 2131**: Dynamic Host Configuration Protocol (DHCP) - https://www.rfc-editor.org/rfc/rfc2131
- **dhcproto crate**: https://crates.io/crates/dhcproto
- **PXE Specification**: Intel Preboot Execution Environment (PXE) Specification

## Example Prompts

### Discovery

```
"Connect to BOOTP server at 192.168.1.1:67 and discover boot information for MAC 00:11:22:33:44:55"
```

### Broadcast Discovery

```
"Send broadcast BOOTP request to find boot servers on network"
```

### PXE Boot Testing

```
"Test PXE boot server at 10.0.0.1 using MAC 52:54:00:12:34:56"
```

### Multiple Server Detection

```
"Discover all BOOTP servers on network and compare their responses"
```

## Command channel (dashboard `[ send ]`)

`AppState::send_to_client` can inject an action into the running client. The channel is
registered in `connect_with_llm_actions` **before** the `bootp_client_connected` LLM call,
because a `manual` routing rule can park that event for minutes and the operator must be
able to reach the client while it waits.

`UdpSocket::recv_from` is cancellation-safe, so the command arm is a `tokio::select!` arm in
the receive loop rather than a separate task — a command never races the loop's own writes,
and losing the race never drops a datagram.

Every verb yields `ClientActionResult::Custom`, so the arm body is bespoke
(`handle_injected_command`) rather than `command_support::handle_stream_client_command`, but
it applies the action through `apply_action_result` — the same function the LLM path uses, so
an injected request is byte-identical to an LLM-produced one and honours the same `broadcast`
flag.

Outcomes:

| Action | Outcome |
|---|---|
| `send_bootp_request` | `Sent { bytes_sent }` — the count `send_to` returned, not the buffer length |
| `disconnect` | `Disconnected`; the receive loop ends (UDP has no socket to shut down) |
| `wait_for_more` | `Executed { detail: "wait_for_more" }` |
| unknown type / bad params | `Rejected { error }` from `execute_action` |

The handle is dropped on every exit path of the receive loop, so the dashboard stops
offering `[ send ]` on a dead client.
