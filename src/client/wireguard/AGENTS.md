# WireGuard VPN Client Implementation

## Overview

Full-featured WireGuard VPN client for connecting to WireGuard VPN servers. This client creates a TUN interface,
establishes an encrypted tunnel, and routes traffic through the VPN connection.

**Status**: Experimental (newly implemented)
**Protocol Spec**: [WireGuard White Paper](https://www.wireguard.com/papers/wireguard.pdf)
**Connection**: UDP-based with automatic handshake management

## Library Choices

### defguard_wireguard_rs v0.7

**Why chosen**:

- Multi-platform unified API (Linux kernel, macOS userspace, Windows kernel, FreeBSD kernel)
- Production-ready Rust library with active maintenance
- Handles all crypto (Curve25519, ChaCha20Poly1305, BLAKE2s)
- Automatic TUN interface creation and management
- Built-in connection monitoring and statistics

**What it provides**:

- `WGApi` - Platform-specific WireGuard API (Kernel on Linux/FreeBSD/Windows, Userspace on macOS)
- `Key` - Curve25519 keypair generation and management
- `Peer` - Server configuration with endpoint, allowed IPs, keepalive
- `InterfaceConfiguration` - Interface setup with addresses, port, MTU
- `read_interface_data()` - Connection status and handshake statistics

**Why not alternatives**:

- `boringtun` - Userspace only, more complex integration
- Manual crypto - WireGuard crypto is complex, library handles it correctly
- Native CLI (`wg`, `wg-quick`) - Violates NetGet architecture (external dependencies)

## Architecture Decisions

### TUN Interface Creation

Platform-specific interface naming:

- **Linux/FreeBSD**: `netget_wg_client{client_id}` (kernel WireGuard)
- **macOS**: `utun20` (wireguard-go userspace)
- **Windows**: `netget_wg_client{client_id}` (kernel WireGuard)

Client assigns itself a VPN IP address (e.g., `10.20.30.2/32`) as specified in startup parameters.

### Monitoring Loop

Spawns async task that polls `read_interface_data()` every 5 seconds:

- Detects successful handshake (connection established)
- Detects handshake timeout (connection lost)
- Updates connection stats (bytes sent/received, last handshake time)
- Calls LLM with connect/disconnect events
- Handles commands from action execution channel

### Command Channel Pattern

WireGuard uses a command channel for action execution:

- Actions like `get_connection_status` and `disconnect` send commands via channel
- Monitoring loop receives and processes commands
- Responses are sent back via oneshot channels
- Global `WIREGUARD_CLIENTS` map stores command senders by client ID

This pattern differs from TCP/HTTP clients which execute actions directly in the read loop.

### Keypair Management

Client generates Curve25519 keypair on startup (or uses provided key):

```rust
let private_key = Key::generate();
let public_key = private_key.public_key();
```

Public key displayed to user for configuration on server side. Server authenticates client with their public key.

## LLM Integration

### Async Actions (User-triggered)

Available anytime from user input:

1. **get_connection_status**: Query current VPN connection status
    - Returns: connection state, handshake time, bytes transferred, endpoint
2. **disconnect**: Disconnect from VPN server
3. **get_client_info**: View client configuration (public key, address, allowed IPs)

### Sync Actions (Network event triggered)

None - WireGuard operates at connection level, not request/response.

### Event Types

- `wireguard_connected`: Successful handshake with server
    - Triggered when first handshake succeeds
    - LLM can respond with status queries or configuration
- `wireguard_disconnected`: Lost connection to server
    - Triggered when handshake timeout (3 minutes)
    - LLM can decide to reconnect or log

## Connection Management

### Connection Establishment

1. Create TUN interface with platform-specific backend
2. Generate or load client keypair
3. Configure interface with client VPN IP and listen port
4. Add server peer with public key, endpoint, and allowed IPs
5. Spawn monitoring loop to track handshake status
6. Call LLM with connected event once handshake succeeds

### Handshake Monitoring

Handshake is considered valid if:

- `peer.last_handshake` is Some
- Last handshake was within 180 seconds (3 minutes)

Monitoring loop tracks handshake state transitions:

- `not_connected` → `connected`: Handshake succeeded
- `connected` → `not_connected`: Handshake timeout

### Command Execution

Commands are sent through global channel storage:

```rust
// Send command
send_command(client_id, WireguardCommand::GetStatus(response_tx)).await?;

// Monitoring loop receives and handles
match cmd {
    WireguardCommand::GetStatus(tx) => {
        let status = client.get_status().await;
        tx.send(status);
    }
    WireguardCommand::Disconnect => {
        client.disconnect().await;
        break;
    }
}
```

### Cleanup

Disconnection removes TUN interface:

```rust
wgapi.remove_interface()?;
```

Command channel cleaned up when monitoring loop exits.

## State Management

### Client Configuration

```rust
pub struct WireguardClientParams {
    server_public_key: String,        // Server's public key (base64)
    server_endpoint: String,          // Server IP:port
    client_address: String,           // Client VPN IP with CIDR (e.g., 10.20.30.2/32)
    allowed_ips: Vec<String>,         // IPs to route through VPN (e.g., ["0.0.0.0/0"])
    keepalive: Option<u16>,           // Keepalive interval in seconds
    private_key: Option<String>,      // Client private key (generated if not provided)
}
```

### Connection Status

```json
{
  "interface": "netget_wg_client0",
  "public_key": "xTIBA5rboUv...",
  "client_address": "10.20.30.2/32",
  "server_endpoint": "1.2.3.4:51820",
  "server_public_key": "abc123...",
  "connected": true,
  "last_handshake": 15,
  "tx_bytes": 1024,
  "rx_bytes": 2048,
  "allowed_ips": ["0.0.0.0/0"]
}
```

## Limitations

### Requires Elevated Privileges

- **Linux/FreeBSD**: Root or `CAP_NET_ADMIN` capability
- **macOS**: root, plus an external `wireguard-go` binary on `PATH` (see System Dependencies) - the userspace backend
  is not privilege-free and is not bundled
- **Windows**: Administrator privileges

### Network Configuration

- **No automatic routing**: Client doesn't configure routing table (WireGuard handles this)
- **Manual server configuration**: Server must add client peer manually (or use LLM)
- **No IPv6**: Currently IPv4-only
- **Static configuration**: No dynamic IP assignment (DHCP)

### Platform-Specific Behaviors

- **macOS**: Uses userspace wireguard-go (slower but works without kernel module)
- **Linux**: Uses kernel WireGuard (fastest, requires kernel 5.6+ or module)
- **Windows**: Uses kernel WireGuard (requires WireGuard driver installed)

### Action Execution

- **Async-only**: No sync actions (no request/response pattern)
- **Command channel**: Actions execute via channel, not direct function calls
- **5-second polling**: Connection status updated every 5 seconds

## Examples

### Client Connection

```
netget> Connect to WireGuard VPN at 1.2.3.4:51820 with server public key xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg= and assign me IP 10.20.30.2/32
```

LLM parses parameters and starts client:

```json
{
  "actions": [
    {
      "type": "open_client",
      "protocol": "wireguard",
      "remote_addr": "1.2.3.4:51820",
      "startup_params": {
        "server_public_key": "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=",
        "server_endpoint": "1.2.3.4:51820",
        "client_address": "10.20.30.2/32",
        "allowed_ips": ["0.0.0.0/0"]
      },
      "instruction": "Connect to VPN"
    }
  ]
}
```

Client output:

```
[CLIENT] WireGuard client 0 public key: abc123...
[CLIENT] Created interface: netget_wg_client0
[CLIENT] Interface configured: netget_wg_client0 → 1.2.3.4:51820
[CLIENT] WireGuard client 0 connected
[CLIENT] WireGuard client 0 handshake successful
```

### Status Query

```
netget> Check VPN connection status
```

LLM calls `get_connection_status` action:

```json
{
  "actions": [
    {
      "type": "get_connection_status"
    }
  ]
}
```

Returns status JSON with connection details.

### Disconnect

```
netget> Disconnect from VPN
```

LLM calls `disconnect` action:

```json
{
  "actions": [
    {
      "type": "disconnect"
    }
  ]
}
```

Client disconnects and removes TUN interface.

## Server Configuration

Server must be configured with client's public key:

```
# On server (netget WireGuard server)
netget> Authorize WireGuard peer with public key abc123... and assign IP 10.20.30.2/32
```

Or using `wg` command:

```bash
sudo wg set netget_wg0 peer abc123... allowed-ips 10.20.30.2/32
```

## System Dependencies

### macOS Setup

**Building** needs nothing special. **Running** needs an external binary, and this section used to claim the opposite
("NO system dependencies ... included in the Rust library itself"), which is false.

`defguard_wireguard_rs`'s macOS backend is a *driver* for the userspace implementation, not the implementation:
`wgapi_userspace.rs` runs `Command::new("wireguard-go")` and then talks to it over
`/var/run/wireguard/<iface>.sock`. That binary is not bundled, and nothing installs it as part of the build. Without it
on `PATH` - and without root - `connect()` fails at interface creation.

```bash
# Build (no system dependency)
./cargo-isolated.sh build --no-default-features --features wireguard

# Run: needs wireguard-go present, and elevation
brew install wireguard-go
sudo netget
```

### Linux Setup

**Requirements**:

- Kernel 5.6+ with WireGuard support, OR
- WireGuard kernel module installed
- `CAP_NET_ADMIN` capability or root privileges

**Installation**:

```bash
# Debian/Ubuntu
sudo apt-get install wireguard-tools linux-headers-$(uname -r)

# Fedora/RHEL
sudo dnf install wireguard-tools kernel-devel-$(uname -r)

# Alpine
apk add wireguard-tools linux-headers

# Arch
sudo pacman -S wireguard-tools linux-headers
```

### Windows Setup

**Requirements**:

- Windows 10 1809+ or Windows 11
- WireGuard Driver installed (included with official WireGuard app)

**Installation**:

```powershell
# Download and install official WireGuard
# https://www.wireguard.com/install/
# OR via Chocolatey:
choco install wireguard
```

### Troubleshooting Platform-Specific Issues

**macOS - "Operation not permitted" error**:

- Ensure you're running the latest version of macOS
- Check System Preferences > Security & Privacy
- The userspace implementation should not require elevation

**Linux - "ioctl(SIOCDEVPRIVATE)..." error**:

- Kernel module not loaded: `modprobe wireguard`
- Missing CAP_NET_ADMIN: Run with `sudo` or grant capability

**Linux - "Interface not found"**:

- Ensure WireGuard tools are installed: `which wg`
- Check if module is loaded: `lsmod | grep wireguard`

## References

- [WireGuard White Paper](https://www.wireguard.com/papers/wireguard.pdf)
- [defguard_wireguard_rs Documentation](https://docs.rs/defguard_wireguard_rs/)
- [WireGuard Protocol Spec](https://www.wireguard.com/protocol/)
- [Curve25519](https://cr.yp.to/ecdh.html)
- [ChaCha20-Poly1305](https://datatracker.ietf.org/doc/html/rfc8439)

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into the running client. `command_loop` in
`mod.rs` drains the bounded channel and runs each action through `apply_wireguard_action`
against the `Arc<WireguardClient>`. The channel is registered **before** the
`wireguard_connected` LLM call and dropped when the interface is torn down or the monitoring
loop exits.

The LLM side goes through the same function. Both event paths - `wireguard_connected` in
`connect_with_llm_actions` and `wireguard_disconnected` in the monitoring loop - used to log
the model's response and execute **none** of its actions, so a client told to check the tunnel
and hang up if it was not established did neither. `apply_llm_actions` now runs each answer
through `apply_wireguard_action`, the same path an injected action takes, and logs what each
one did.

**Neither path is covered by a test here**, for the same reason the injection path is not:
`connect()` creates a real network interface, so it cannot succeed without root and, on macOS,
`wireguard-go`. Nothing is mocked to manufacture a pass - that is what cost this protocol its
Stable rating.

Outcomes:

| action | `ClientSendOutcome` |
|---|---|
| `get_connection_status` | `Executed { detail: "get_connection_status: connected=…, tx_bytes=…, rx_bytes=…, last_handshake=…" }` |
| `get_client_info` | `Executed { detail: "get_client_info: interface=…, public_key=…, …" }` |
| `disconnect` | `Disconnected` (interface removed, handle dropped) |
| unknown verb | `Rejected { error }` |

**WireGuard can never report `Sent`.** NetGet implements none of the WireGuard protocol and
writes no packets — it orchestrates `defguard_wireguard_rs`, which drives the kernel module
(or `wireguard-go` on macOS). Every verb this client has reads interface state or tears the
interface down.

**Untested here, deliberately.** `configure_interface` creates a real network interface, which
needs root and, on macOS, an external `wireguard-go` binary — so `connect()` cannot succeed in
a normal test process and the injection path has never been executed. `tests/client/wireguard/
command_channel_test.rs` asserts only what is reachable unprivileged (a failed connect leaves
no command handle and `send_to_client` refuses) and marks the privileged half `#[ignore]`. It
does **not** mock an action or event to manufacture a pass; that is what cost this protocol its
Stable rating.
