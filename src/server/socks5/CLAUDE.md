# SOCKS5 Proxy Protocol Implementation

## Overview

SOCKS5 proxy server implementing RFC 1928 (SOCKS Protocol Version 5) with LLM-controlled connection filtering and
optional Man-in-the-Middle (MITM) traffic inspection. Supports IPv4, IPv6, and domain name resolution with flexible
authentication.

**Compliance**: RFC 1928 (SOCKS5), RFC 1929 (Username/Password Authentication)

## Security: this is an unrestricted open relay (read this first)

The destination of every connection is chosen by the **peer**, and there is no
allow-list, deny-list or network restriction of any kind in this protocol.
Loopback, link-local — including `169.254.169.254`, the cloud instance-metadata
endpoint — and every RFC 1918 range are reachable. Anyone who can reach this port
can reach whatever this host can:

- **SSRF pivot** into the operator's private network.
- **Open relay** someone else's traffic can be laundered through, attributed to
  this machine.

That may be exactly what you want from a honeypot. It is written down here because
nothing in the code refuses it.

**The filter configuration does not change this.** `target_host_patterns` and
`target_port_ranges` decide what the model is *asked* about; they restrict nothing
on their own. `filter_mode: allow_all` connects with no consultation at all, and
`selective` with `default_action: "allow"` does the same for every target the
patterns miss. The model is the only gate, and only when it is consulted.

Two more things the wire does not carry:

- SOCKS5 has exactly one refusal code (`0x02`, connection not allowed), so a peer
  cannot tell "the model refused you" from "the backend was down". The log can:
  `decision=model_reject` / `model_silent` / `model_allow` /
  `fail_closed_llm_error`, following `src/server/radius/`.
- The handshake reads time out after 30s (`HANDSHAKE_TIMEOUT_SECS`), so a peer
  that connects and stalls no longer holds a task forever. The number of
  concurrent connections is still unbounded.

## Library Choices

**Manual Implementation** - Complete SOCKS5 protocol implemented from scratch

- **Why**: Maximum flexibility for LLM control over authentication and connection decisions
- No Rust SOCKS5 server libraries provide the granular action-based control needed
- Allows custom filtering, MITM inspection, and policy enforcement

**Protocol Parsing**:

- Manual binary protocol parsing for SOCKS5 handshake, auth, and CONNECT requests
- Efficient byte-level operations with tokio AsyncReadExt/AsyncWriteExt
- Clear separation of handshake phases for debugging

## Architecture Decisions

### Four-Phase Connection Lifecycle

**Phase 1: Handshake (Authentication Method Negotiation)**

```
Client → Server: [VER=5, NMETHODS, METHODS...]
Server → Client: [VER=5, METHOD]
```

- Client proposes authentication methods (0x00=no auth, 0x02=username/password)
- Server selects method based on `Socks5FilterConfig.auth_methods`
- LLM not consulted (fast path)

**Phase 2: Authentication (if required)**

```
Client → Server: [VER=1, ULEN, USERNAME, PLEN, PASSWORD]
Server → Client: [VER=1, STATUS]
```

- If username/password selected, client sends credentials
- **LLM consulted** via `SOCKS5_AUTH_REQUEST_EVENT` to validate credentials
- LLM returns success/failure → Server sends 0x00 (success) or 0x01 (failure)

**Phase 3: CONNECT Request**

```
Client → Server: [VER=5, CMD=1, RSV=0, ATYP, DST.ADDR, DST.PORT]
Server → Client: [VER=5, REP, RSV=0, ATYP, BND.ADDR, BND.PORT]
```

- Client requests connection to target (IPv4, IPv6, or domain)
- **LLM consulted** via `SOCKS5_CONNECT_REQUEST_EVENT` (if filter matches)
- LLM decides: allow/deny + optional MITM flag
- Server sends success (0x00) or failure (0x02=connection not allowed)

**Phase 4: Data Relay**

- **Pass-through mode**: Direct bidirectional copy between client ↔ target (tokio::io::copy_bidirectional)
- **MITM mode**: Inspect each data chunk via LLM (`SOCKS5_DATA_TO_TARGET_EVENT`, `SOCKS5_DATA_FROM_TARGET_EVENT`)

### Filter Configuration System

**`Socks5FilterConfig`** controls LLM involvement:

```rust
pub struct Socks5FilterConfig {
    auth_methods: Vec<u8>,              // [0x00, 0x02] = support both no-auth and user/pass
    filter_mode: FilterMode,            // AllowAll, DenyAll, AskLlm, Selective
    target_host_patterns: Vec<String>,  // Regex patterns for selective filtering
    target_port_ranges: Vec<(u16, u16)>, // Port ranges for selective filtering
    default_action: String,             // "allow" or "deny" when not matching patterns
    mitm_by_default: bool,              // Enable MITM for allowed connections
}
```

**FilterMode Behavior**:

- `AllowAll`: All connections allowed without LLM consultation (fast proxy mode)
- `DenyAll`: All connections denied (honeypot/monitoring mode)
- `AskLlm`: Every connection consults LLM (maximum control)
- `Selective`: LLM consulted only when target matches `target_host_patterns` or `target_port_ranges`

This prevents LLM overhead for non-interesting traffic while maintaining control over sensitive destinations.

### Target Address Types

**`TargetAddr` enum** supports all SOCKS5 address types:

```rust
pub enum TargetAddr {
    Ipv4(Ipv4Addr, u16),      // ATYP=0x01: Direct IPv4 address
    Ipv6(Ipv6Addr, u16),      // ATYP=0x04: Direct IPv6 address
    Domain(String, u16),      // ATYP=0x03: Domain name (proxy resolves DNS)
}
```

Domain resolution performed by proxy, not client. This allows:

- DNS-based filtering (block connections to specific domains)
- DNS monitoring (log all domain lookups)
- DNS manipulation (redirect domains to honeypot servers)

## LLM Integration

### Action-Based Control

**Authentication Decision** (`SOCKS5_AUTH_REQUEST_EVENT`):

```json
{
  "actions": [
    {
      "type": "allow_socks5_auth",
      "username": "user123",
      "message": "Credentials valid"
    }
  ]
}
```

- LLM can validate credentials against any policy (database, LDAP, custom logic)
- The decision is read from the **action name**: `allow_socks5_auth` permits,
  `deny_socks5_auth` (or emitting neither) rejects. It used to be inferred from
  "any result is `ActionResult::NoAction`", which several unrelated actions also
  return, so an unrelated action could authorise the session.
- **Fail closed**: an event that produces no decision action is denied

**Connection Decision** (`SOCKS5_CONNECT_REQUEST_EVENT`):

```json
{
  "actions": [
    {
      "type": "allow_socks5_connect",
      "target": "example.com:443",
      "mitm": true,
      "message": "Allowing with inspection"
    },
    {
      "type": "deny_socks5_connect",
      "target": "malware-c2.com:8080",
      "reason": "Known malicious domain"
    }
  ]
}
```

- `mitm: true` → Enable MITM inspection for this connection
- `mitm: false` → Fast pass-through mode
- The decision is read from the action name (`allow_socks5_connect` /
  `deny_socks5_connect`); an explicit deny wins, and no decision means deny

**Data Inspection** (MITM mode only):

`SOCKS5_DATA_TO_TARGET_EVENT` (client → target):

```json
{
  "actions": [
    {
      "type": "forward_socks5_data"
    },
    {
      "type": "modify_socks5_data",
      "data": "48656c6c6f",
      "encoding": "hex"
    },
    {
      "type": "close_connection",
      "reason": "Blocked malicious payload"
    }
  ]
}
```

`SOCKS5_DATA_FROM_TARGET_EVENT` (target → client): Same structure

**There is no `data_base64` field and no `close_socks5_connection` action** — the
earlier docs listed both. `modify_socks5_data` takes `data` plus an optional
`encoding` (`"utf8"` default, or `"hex"`); base64 was documented but never decoded,
so a base64 payload was relayed as its literal base64 text. The close action is
named `close_connection`.

**Binary payloads**: the event reports `data` together with an `encoding` field —
printable payloads arrive as text (`"utf8"`), anything else arrives hex-encoded
(`"hex"`). Previously the payload was pushed through `String::from_utf8_lossy`,
which replaced every non-UTF-8 byte with U+FFFD and made binary traffic
unrecoverable and unmodifiable.

### Event Types

1. **`SOCKS5_AUTH_REQUEST_EVENT`**
    - Triggered: Username/password authentication phase
    - Context: username, password
    - LLM decides: `allow_socks5_auth` or `deny_socks5_auth` (no action = deny)

2. **`SOCKS5_CONNECT_REQUEST_EVENT`**
    - Triggered: CONNECT request (if filter matches)
    - Context: target (host:port), username (if authenticated)
    - LLM decides: `allow_socks5_connect` (optional `mitm`) or `deny_socks5_connect`
      (no action = deny)

3. **`SOCKS5_DATA_TO_TARGET_EVENT`** (MITM mode)
    - Triggered: Each data chunk from client to target
    - Context: data, encoding, target, username
    - LLM decides: `forward_socks5_data`, `modify_socks5_data`, `close_connection`

4. **`SOCKS5_DATA_FROM_TARGET_EVENT`** (MITM mode)
    - Triggered: Each data chunk from target to client
    - Context: data, encoding, target, username
    - LLM decides: `forward_socks5_data`, `modify_socks5_data`, `close_connection`

## Connection and State Management

**Per-Connection State**: not in `ProtocolConnectionInfo`. That type is a generic
`serde_json::Value` wrapper (`src/state/server.rs`), not an enum, and SOCKS5
registers `ProtocolConnectionInfo::empty()` — there is no `Socks5` variant. The
target address and username are pushed into the connection entry by
`AppState::update_socks5_target`; the Idle/Processing/Accumulating machine and any
queued data live in the module's own private types.

**Connection Lifecycle**:

1. Accept TCP connection → Add to server connections
2. Phase 1: Handshake → Select auth method (fast)
3. Phase 2: Authentication (if needed) → LLM consultation
4. Phase 3: CONNECT request → LLM consultation (if filtered)
5. Connect to target server. On failure the client gets a SOCKS5 reply carrying the
   mapped failure code (0x05 connection refused, 0x04 host unreachable, 0x03 network
   unreachable, else 0x01). The connection used to be dropped with no reply at all,
   leaving clients waiting for the CONNECT response until they timed out.
6. Phase 4: Relay data (pass-through or MITM)
7. Connection closes → Mark as closed

**Concurrent Connections**: Each connection handled in a separate tokio task. No
limit is enforced. Handshake reads time out after 30s, which bounds how long a
silent peer can hold one, but not how many peers can arrive.

## MITM Inspection Mode

### When to Use MITM

**Trade-off**: MITM adds significant latency (LLM call per data chunk) but provides complete visibility.

**Use Cases**:

- Malware analysis: Inspect all traffic to/from suspicious destinations
- Data exfiltration detection: Scan outbound data for sensitive patterns
- Protocol analysis: Decode and log application protocols (HTTP, custom protocols)
- Forensics: Capture full plaintext traffic for investigation

**Performance Impact**:

- Pass-through: ~1-2ms overhead per connection
- MITM: 500ms-5s per data chunk (2 LLM calls: to target, from target)

**Implementation**:

```rust
loop {
    tokio::select! {
        // Read from client
        result = client_stream.read(&mut buf) => {
            // Consult LLM, forward modified data to target
        }
        // Read from target
        result = target_stream.read(&mut buf) => {
            // Consult LLM, forward modified data to client
        }
    }
}
```

## Protocol Compliance

### Supported Features

- ✅ SOCKS5 handshake (RFC 1928)
- ✅ No authentication (0x00)
- ✅ Username/password authentication (0x02, RFC 1929)
- ✅ CONNECT command (0x01)
- ✅ IPv4 addresses (ATYP=0x01)
- ✅ IPv6 addresses (ATYP=0x04)
- ✅ Domain names (ATYP=0x03)

### Not Implemented

- ❌ BIND command (0x02) - Server listens for inbound connections
- ❌ UDP ASSOCIATE command (0x03) - UDP relay
- ❌ GSSAPI authentication (0x01)
- ❌ Other authentication methods

**Rationale**: CONNECT is 99% of real-world SOCKS5 usage. BIND/UDP ASSOCIATE rarely used, complex to implement
correctly.

## Limitations

### Current Limitations

1. **No UDP Support**
    - Only TCP connections (CONNECT command)
    - UDP ASSOCIATE not implemented
    - Games, VoIP, and DNS-over-UDP won't work

2. **No BIND Support**
    - FTP active mode won't work
    - Some P2P protocols require BIND

3. **MITM Performance**
    - Each data chunk requires LLM consultation
    - Not suitable for high-throughput applications (video streaming, large downloads)
    - Consider pass-through mode for performance-sensitive traffic

4. **`username_patterns` config field is dead**
    - `Socks5FilterConfig.username_patterns` is parsed and stored but never read by
      any filtering decision

5. **No Connection Pooling**
    - Each SOCKS5 connection creates new target connection
    - No HTTP/1.1 keep-alive equivalent
    - May exhaust ports under high load

### Security Considerations

**Authentication**: Username/password sent in plaintext over SOCKS5 connection (not TLS). Use SOCKS5 over SSH tunnel for
encrypted credentials.

**MITM Mode**: Breaks end-to-end encryption if used with HTTPS/TLS (user sees cleartext). Use carefully and with user
consent.

**DNS Leaks**: Proxy resolves domain names, preventing client DNS leaks. Good for privacy.

## Example Prompts

### Basic Proxy (No Auth, Allow All)

```
Listen on port 1080 using SOCKS5 stack with no authentication. Allow all connections.
```

### Allow All with Logging

```
Listen on port 1080 using SOCKS5 stack. Allow all connections but log the target
destination for each connection.
```

### Username/Password Authentication

```
Listen on port 1080 using SOCKS5 stack with username/password authentication.
Accept username "admin" with password "secret123". Deny all others.
```

### Selective Blocking

```
Listen on port 1080 using SOCKS5 stack. Block connections to *.facebook.com and
*.twitter.com. Allow all other connections.
```

### MITM Inspection for Specific Domain

```
Listen on port 1080 using SOCKS5 stack. For connections to api.suspicious.com,
enable MITM inspection and log all data. Use pass-through for all other connections.
```

### Port-Based Filtering

```
Listen on port 1080 using SOCKS5 stack. Allow connections to ports 80 and 443 only.
Block all other ports with reason "Only HTTP/HTTPS allowed".
```

### Honeypot Mode

```
Listen on port 1080 using SOCKS5 stack. Accept all connections but log full details
(source IP, username, target, all data). Enable MITM for all connections.
```

## Performance Considerations

**Pass-Through Mode**: Near-zero CPU overhead after connection establishment; the
relay is `tokio::io::copy_bidirectional`, so no per-connection buffers are held
here at all. MITM mode allocates 2 × 8 KiB per connection.

**Selective Filtering**: Regex pattern matching adds ~10-50μs per connection.

**MITM Mode**: High latency (LLM calls). Not recommended for >10 concurrent connections or high-bandwidth usage.

**Concurrent Connections**: Tokio async allows thousands of connections. Practical limit: LLM throughput (1-10
queries/second).

## References

- RFC 1928: SOCKS Protocol Version 5
- RFC 1929: Username/Password Authentication for SOCKS V5
- SOCKS5 Protocol Specification: https://datatracker.ietf.org/doc/html/rfc1928
- Common SOCKS5 Issues: https://en.wikipedia.org/wiki/SOCKS#SOCKS5
