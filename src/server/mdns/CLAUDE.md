# mDNS Protocol Implementation

## Overview

mDNS/DNS-SD (Multicast DNS / DNS Service Discovery) server for zero-configuration network service advertisement.
Implements RFC 6762 (mDNS) and RFC 6763 (DNS-SD) using the mdns-sd library.

**Status**: Experimental. The LLM's only control point is a single startup event;
there is no query handling and no runtime reconfiguration. `metadata()` said `Beta`
until September 2026 while this file said Experimental - the code was the wrong half,
and it is now Experimental in both places. See "What the tests prove" below.
**Port**: announces on 224.0.0.251:5353. No listening socket of its own is
bound, and 5353 is unprivileged, so `PrivilegeRequirement::None` is correct.
**Tests**: `tests/server/mdns/test.rs` (note: `test.rs`, not `e2e_test.rs`),
four mock-driven tests that browse for the advertised services.

### What the tests prove

Each test browses the group with an `mdns-sd` daemon and asserts **its own** instance
resolves, by name, with its TXT properties intact. That covers the registration
plumbing end to end: `startup_params` (or a `register_mdns_service` action) ->
`ServiceInfo` -> the multicast group -> a browser that reads it back.

It is not evidence of interop, because `mdns-sd` is also the crate this server
registers through - one crate round-tripping through itself, the reason `websocket`
and `webrtc_signaling` are held at Experimental and the reason `rss` had to switch to
`feed-rs` before it could be promoted. Beta means "works against real clients"; the
independent client here would be the platform tool (`dns-sd -L` on macOS,
`avahi-browse -r` on Linux), and until one of those drives a test this stays
Experimental.

The tests asserted *nothing at all* before September 2026 - they computed a
`found_service` flag and printed it - and `metadata().e2e_testing` described them as
"asserting on ServiceResolved". Both are fixed.

## Library Choices

- **mdns-sd** v0.15 (see `Cargo.toml`) - Full mDNS/DNS-SD implementation
    - Handles multicast group management (224.0.0.251:5353)
    - Service advertisement and discovery
    - TXT record management
    - Automatic response caching and conflict resolution
- Chosen because mDNS/DNS-SD is complex (multicast, timing, caching)
- Library handles protocol details, LLM focuses on service registration

## Architecture Decisions

### Service Advertisement Model

mDNS is **advertisement-only** in NetGet:

- Server starts, LLM registers services via `register_mdns_service` action
- No incoming request handling (mDNS is announcement-based)
- Services continuously advertised until server shutdown
- No sync actions - only async `register_mdns_service`

### LLM Integration

- **Single event type**: `MDNS_SERVER_STARTUP_EVENT` (`mdns_server_startup`)
- Triggered once when mDNS server initializes, and only if no `startup_params`
  were supplied - passing `service_type` or `services` registers the services
  directly and skips the model call entirely
- The call goes through `call_llm`, so a configured script or static handler for
  `mdns_server_startup` runs in-process with no model call
- LLM returns one or more `register_mdns_service` actions
- **Manual action processing** - actions are read out of
  `execution_result.raw_actions` in `spawn_with_llm_actions()` and registered
  against the live `ServiceDaemon` there
- `MdnsProtocol::execute_action` therefore does **not** register anything. It
  returns `ActionResult::NoAction` for `register_mdns_service`, because outside
  the startup pass (e.g. if the action is issued as a user-triggered async
  action later) there is no daemon handle to register against. Registering a
  service after startup is not supported.

### Startup Parameters

Supplied via `open_server`'s `startup_params`; when present the LLM is not
consulted at all.

- `service_type` / `service_name` / `port` / `properties` - register one service
- `services` - array of `{service_type, service_name, port, properties}` objects
- `port` defaults to the server's own port, or 8080 when that is 0. It is the
  port published in the SRV record - i.e. the port discovering clients will
  connect to - and is unrelated to mDNS itself, which always uses 5353.

### Service Registration Flow

1. Server startup triggers `MDNS_SERVER_STARTUP_EVENT`
2. LLM returns actions: `[{type: "register_mdns_service", ...}, ...]`
3. Code extracts `raw_actions` from execution result
4. For each action, create `ServiceInfo` with:
    - Service type (e.g., `_http._tcp.local.`)
    - Instance name (e.g., `My Web Server`)
    - Host name (generated from instance name)
    - Port number
    - TXT properties (key-value pairs)
5. Register service with `mdns.register(service_info)`
6. Service continuously advertised on multicast address

### Local IP Detection

Uses a heuristic to find the local IP:

```rust
fn get_local_ip() -> Option<String> {
    // Bind a UDP socket and "connect" to 192.0.2.1:80 (RFC 5737 TEST-NET-1).
    // connect() on UDP only does a routing-table lookup - no packets are sent
    // and the destination is never contacted - so the socket then reports the
    // local IP the default route would use.
    // Fallback to 127.0.0.1 if detection fails.
}
```

The destination is a documentation address rather than a real host so that the
lookup carries no dependency on, or traffic implication for, a third party.

## Connection Management

- **No connections** - mDNS is multicast-based
- No TCP/UDP listener
- `ServiceDaemon` runs in a background tokio task, parked on
  `std::future::pending()`
- The task's `JoinHandle` is registered with `AppState::register_server_task`,
  so `stop_server` can abort it
- Address returned to the caller: `0.0.0.0:0`, this codebase's "I listen on
  nothing" placeholder, which `server_startup::is_bound_addr` recognises and
  declines to advertise. It used to return `224.0.0.251:5353`, which the TUI and
  `server_status` then displayed as if it were a listening endpoint. It is not one:
  joining a group is not binding it, and nobody owns it. The group is reported on
  the status stream instead, as information

## State Management

- **No state** - Service registrations managed by mdns-sd library
- No tracking in `AppState`
- Services automatically re-announced periodically per RFC 6762
- Daemon shutdown: `mdns_sd::ServiceDaemon` has **no** `Drop` impl, so dropping
  it leaves the background thread announcing. The task therefore holds a
  `DaemonGuard` whose `Drop` calls `ServiceDaemon::shutdown()`; that runs when
  the task is aborted, which is what actually stops the announcements.

## Service Types

Common service types:

- `_http._tcp.local.` - HTTP web server
- `_https._tcp.local.` - HTTPS web server
- `_ftp._tcp.local.` - FTP server
- `_ssh._tcp.local.` - SSH server
- `_printer._tcp.local.` - Printer service
- `_smb._tcp.local.` - SMB/CIFS file sharing
- Custom types: `_myapp._tcp.local.`

## TXT Properties

Optional key-value pairs advertised with service:

- `txtvers=1` - TXT record version
- `path=/api` - Service path
- `version=2.0` - Application version
- `secure=true` - Security flag
- Any custom properties

## Limitations

- **Advertisement only** - No mDNS query handling reaches the LLM. Incoming
  queries for the advertised services are answered by mdns-sd internally; there
  is no `mdns_query` event, so instructions like "respond differently to
  queries from X" cannot be honoured.
- **No dynamic updates** - Services registered at startup only.
  `register_mdns_service` issued after startup is a no-op (see LLM Integration).
- **No service unregistration** - Services advertised until shutdown
- **No conflict resolution visibility** - Handled by library
- **No IPv6 support** - IPv4 only (could be added)
- **No custom TTL** - Uses library defaults
- **No service browsing** - NetGet doesn't browse for other services

## Examples

### Example LLM Prompt

```
listen on port 8080 via mdns. Advertise HTTP service:
- type: _http._tcp.local.
- name: NetGet Web Server
- port: 8080
- properties: version=1.0, path=/
```

### Example LLM Response (Single Service)

```json
{
  "actions": [
    {
      "type": "register_mdns_service",
      "service_type": "_http._tcp.local.",
      "instance_name": "NetGet Web Server",
      "port": 8080,
      "properties": {
        "version": "1.0",
        "path": "/"
      }
    }
  ]
}
```

### Example LLM Response (Multiple Services)

```json
{
  "actions": [
    {
      "type": "register_mdns_service",
      "service_type": "_http._tcp.local.",
      "instance_name": "Web Server",
      "port": 8080,
      "properties": {
        "path": "/"
      }
    },
    {
      "type": "register_mdns_service",
      "service_type": "_ftp._tcp.local.",
      "instance_name": "FTP Server",
      "port": 21,
      "properties": {
        "version": "1.0"
      }
    }
  ]
}
```

## Technical Details

### Multicast DNS

- **Multicast group**: 224.0.0.251 (IPv4)
- **Port**: 5353
- **TTL**: Typically 255 (link-local)
- **Announcement frequency**: Initial burst (3x), then periodic

### DNS-SD Records

For service `My Web Server._http._tcp.local.`:

- **PTR** record: `_http._tcp.local.` → `My Web Server._http._tcp.local.`
- **SRV** record: hostname + port
- **TXT** record: key=value properties
- **A** record: IPv4 address

### Service Discovery

Other devices discover services by:

1. Querying `_http._tcp.local.` PTR records
2. Receiving instance names
3. Querying instance SRV/TXT/A records
4. Connecting to advertised IP:port

## References

- RFC 6762 - Multicast DNS
- RFC 6763 - DNS-Based Service Discovery
- mdns-sd documentation: https://docs.rs/mdns-sd
- Apple Bonjour: https://developer.apple.com/bonjour/
