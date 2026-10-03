# mDNS Client Implementation

## Overview

The mDNS (Multicast DNS) client enables LLM-controlled service discovery on the local network. It implements RFC 6762 (
mDNS) and RFC 6763 (DNS-SD) for zero-configuration networking, allowing discovery of services like HTTP servers,
printers, SSH hosts, and other networked devices advertising via mDNS/Bonjour/Zeroconf.

## Library Choice

**Primary Library**: `mdns-sd` v0.11+

**Rationale**:

- Pure Rust implementation of mDNS and DNS-SD
- Daemon-based architecture with channel-based event delivery
- Supports both synchronous and asynchronous patterns
- RFC-compliant (RFC 6762, RFC 6763)
- Active maintenance and good documentation
- No external C library dependencies

**Alternative Considered**:

- `simple-mdns`: Requires explicit `async-tokio` feature, less mature API
- `libmdns`: Responder-focused, less suitable for client discovery
- `zeroconf-tokio`: Wrapper around C libraries, platform-specific dependencies

## Architecture

### Connection Model

mDNS is **connectionless** (multicast UDP on 224.0.0.251:5353), but the client maintains:

- **Active listening state**: ServiceDaemon runs in background thread
- **Event-driven discovery**: Browse operations spawn tasks listening for service events
- **Channel-based communication**: ServiceDaemon communicates via `flume` channels
- **Client lifecycle**: Open client → browse/resolve → close client

### Service Discovery Flow

```
1. Initialize ServiceDaemon
   ↓
2. LLM receives "mdns_connected" event
   ↓
3. LLM decides to browse for "_http._tcp.local"
   ↓
4. Spawn task listening for ServiceEvent
   ↓
5. On ServiceEvent::ServiceFound:
   - Call LLM with "mdns_service_found" event
   - LLM decides to wait for resolution or query more
   ↓
6. On ServiceEvent::ServiceResolved:
   - Call LLM with "mdns_service_resolved" event (IP, port, TXT properties)
   - LLM analyzes service details
```

### State Management

The client is **stateless** from NetGet's perspective:

- ServiceDaemon maintains internal mDNS state
- No connection state machine (Idle/Processing/Accumulating) needed
- Each browse operation spawns an independent event-listening task
- Multiple concurrent browse operations are supported

## LLM Integration

### Actions

**Async Actions** (user-triggered):

1. **browse_service**
    - Browse for services of a specific type
    - Parameter: `service_type` (e.g., "_http._tcp.local", "_ssh._tcp.local", "_printer._tcp.local")
    - Spawns task to listen for service discovery events
    - Common service types: `_http._tcp.local`, `_ssh._tcp.local`, `_smb._tcp.local`, `_printer._tcp.local`,
      `_airplay._tcp.local`

2. **resolve_hostname**
    - Resolve a .local hostname to IP addresses
    - Parameter: `hostname` (e.g., "myserver.local")
    - Uses mDNS query with 5-second timeout
    - Returns list of IP addresses

3. **disconnect**
    - Stop service discovery and clean up

**Sync Actions** (response to events):

1. **wait_for_more**
    - Wait for more service discovery events
    - Used when LLM wants to see multiple services before acting

### Events

1. **mdns_connected**
    - Triggered on successful initialization
    - LLM decides what to browse/resolve

2. **mdns_service_found**
    - Triggered when a service instance is discovered
    - Contains: `service_type`, `fullname`
    - LLM can decide to wait for resolution or continue browsing

3. **mdns_service_resolved**
    - Triggered when service is fully resolved
    - Contains: `fullname`, `hostname`, `addresses` (array), `port`, `properties` (TXT records)
    - LLM analyzes service details and can initiate connections

### Event Processing

The implementation uses a **spawn-per-browse** model:

- Each `browse_service` action spawns a dedicated task
- Task listens on the `flume::Receiver<ServiceEvent>` channel
- Uses `recv_timeout(10s)` to avoid blocking forever
- On timeout, checks if client still exists before continuing
- Terminates when `SearchStopped` event received or client removed

## Implementation Details

### Multicast Group

- **IPv4**: 224.0.0.251:5353
- **IPv6**: ff02::fb:5353
- Requires multicast-capable network interface
- Works on LAN only (TTL=1)

### Service Type Format

Standard format: `_service._proto.domain`

- Service: Human-readable service name (e.g., `http`, `ssh`, `printer`)
- Proto: Transport protocol (`tcp` or `udp`)
- Domain: Always `.local` for mDNS

Examples:

- `_http._tcp.local` - HTTP web servers
- `_ssh._tcp.local` - SSH servers
- `_smb._tcp.local` - SMB/CIFS file shares
- `_printer._tcp.local` - Network printers
- `_airplay._tcp.local` - AirPlay devices

### TXT Properties

Services can include key-value metadata:

```rust
info.get_properties().iter().map(|p| format!("{}={}", p.key(), p.val_str()))
```

Common TXT properties:

- `path=/admin` (HTTP path)
- `txtvers=1` (TXT record version)
- `model=MacBookPro` (Device model)

## Logging Strategy

Dual logging (tracing macros + status_tx):

- **INFO**: Service resolution, hostname lookups
- **DEBUG**: Service discovery lifecycle
- **TRACE**: Individual service found events, search started/stopped
- **WARN**: Resolution failures, unknown actions
- **ERROR**: LLM errors, browse failures

Examples:

```rust
info!("mDNS client {} browsing for service: {}", client_id, service_type);
trace!("mDNS service found: {} ({})", fullname, service_type);
error!("mDNS browse error: {}", e);
```

## Limitations

### 1. Local Network Only

- mDNS is restricted to link-local multicast (TTL=1)
- Cannot discover services across routers
- Requires multicast-capable network

### 2. Platform Differences

- macOS: Built-in mDNS responder (mDNSResponder/Bonjour)
- Linux: Requires Avahi daemon or systemd-resolved
- Windows: Requires Bonjour Print Services or similar

### 3. Discovery Timing

- Services may take 1-10 seconds to be discovered
- Resolution can timeout (5 seconds default)
- Race conditions possible in fast networks

### 4. No Service Registration

- Client implementation is **discovery-only**
- Cannot register/advertise services
- For service registration, use mDNS server protocol

### 5. Concurrent Browsing

- Multiple simultaneous browse operations spawn multiple tasks
- Can lead to redundant LLM calls for the same service
- Consider implementing deduplication if needed

## Security Considerations

### 1. Local Network Trust

- mDNS assumes trusted local network
- No authentication or encryption
- Services can be spoofed by local attackers

### 2. Information Disclosure

- Service discovery reveals network topology
- TXT properties may contain sensitive info
- Consider privacy implications

### 3. Denial of Service

- Malicious responders can flood with fake services
- No rate limiting on service discovery events
- LLM call budget can be exhausted

## Testing Recommendations

### Unit Testing

- Mock ServiceDaemon with test fixtures
- Test action parsing and event generation
- Verify error handling

### E2E Testing

- Use built-in mDNS responders (macOS/Linux)
- Test browsing for known services (e.g., `_http._tcp.local`)
- Limit LLM calls to < 5 per test
- Budget: 30-60 seconds per test

### Example Test Scenario

```
1. Start mDNS client
2. Browse for "_http._tcp.local"
3. Wait for ServiceResolved event (timeout 10s)
4. Verify LLM receives service details
5. Cleanup
```

## Common Service Types Reference

| Service Type             | Description           | Typical Port |
|--------------------------|-----------------------|--------------|
| `_http._tcp.local`       | HTTP web servers      | 80, 8080     |
| `_https._tcp.local`      | HTTPS web servers     | 443, 8443    |
| `_ssh._tcp.local`        | SSH servers           | 22           |
| `_smb._tcp.local`        | SMB/CIFS file shares  | 445          |
| `_afpovertcp._tcp.local` | Apple Filing Protocol | 548          |
| `_printer._tcp.local`    | Network printers      | 631          |
| `_ipp._tcp.local`        | IPP printers          | 631          |
| `_airplay._tcp.local`    | AirPlay devices       | Various      |
| `_raop._tcp.local`       | Remote Audio Output   | Various      |
| `_homekit._tcp.local`    | HomeKit devices       | Various      |

## Example Prompts

1. **Basic Discovery**
    - "Browse for HTTP services on the local network"
    - "Find all SSH servers using mDNS"

2. **Hostname Resolution**
    - "Resolve myserver.local to IP address"
    - "Find the IP of raspberrypi.local"

3. **Service Analysis**
    - "Discover all printers and show their capabilities"
    - "List all web servers with their paths and ports"

4. **Specific Service Discovery**
    - "Find AirPlay devices and show their models"
    - "Discover SMB shares on the network"

## Dependencies

```toml
[dependencies]
mdns-sd = { version = "0.11", optional = true }
```

**Transitive dependencies**:

- `flume` - MPSC channels for event delivery
- `socket2` - Low-level socket operations
- `nix` - Unix system calls (Linux/macOS)

**Feature flag**: `mdns`

## Command channel (dashboard `[ send ]`)

`AppState::send_to_client` can inject an action into the running client. The channel is
registered **before** the `mdns_client_connected` LLM call (a `manual` routing rule can park
that event for minutes), and the commands are drained by a task of their own
(`command_loop`), registered with `AppState::register_client_task`.

The command task holds a **clone of the same `ServiceDaemon` handle**, not a second daemon.
`ServiceDaemon` is a cloneable handle onto one running daemon, so an injected `browse_service`
leaves the same sockets, for the same multicast group, as one the LLM asked for. It also keeps
the daemon alive when the client has no instruction at all — previously the daemon was dropped
in that case.

**Outcomes are `Executed`, never `Sent`, and that is the honest shape here.** `mdns-sd` owns
the multicast socket and reports neither the query bytes it emits nor when it emits them, so
any byte count this client produced would be invented. The `detail` names what the daemon was
asked to do instead:

| Action | Outcome |
|---|---|
| `browse_service` | `Executed { detail: "browse_service '<type>' started (the mdns-sd daemon owns the multicast socket, so no byte count is observable)" }` |
| `resolve_hostname` | `Executed { detail: "resolve_hostname '<host>' issued" }`, or `Err` when the daemon call fails |
| `disconnect` | `Disconnected`; the command loop ends |
| `wait_for_more` | `Executed { detail: "wait_for_more" }` |
| unknown type / bad params | `Rejected { error }` from `execute_action` |

Note that `resolve_hostname` blocks the command loop for up to its 5s daemon timeout, so a
second injected command queues behind it.

### Two things found while wiring the command channel

- **The browse task's `recv_timeout` was a synchronous blocking call inside
  `tokio::spawn`.** That is the tokio-blocking-in-async pattern the root `CLAUDE.md` lists
  under known systemic issues, and it bit exactly as described: it parks a runtime worker for
  up to 10 seconds per iteration, so on a current-thread runtime everything else the client is
  doing — the command channel included — stalls behind it. It now runs on the blocking pool
  via `tokio::task::spawn_blocking`; the match arms are unchanged.
- **`resolve_hostname` does not return addresses.** `ServiceDaemon::resolve_hostname` returns
  a `Receiver<HostnameResolutionEvent>`, and this client binds it as `addrs` and logs
  `addrs.len()` — the channel's queue depth, which is 0. So the "Resolved <host> to N
  addresses" line is always "to 0 addresses" and no address is ever read. Not fixed here; the
  command channel's outcome detail says `resolve_hostname '<host>' issued`, which is what
  actually happened.
