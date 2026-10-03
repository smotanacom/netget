# RIP Server Implementation

## Overview

Routing Information Protocol version 2 (RIPv2) server implementing RFC 2453. RIP is a distance-vector routing protocol.

### Default behaviour: static (advertise nothing), no LLM

A RIP response is **not wire-determined** — which routes to advertise (and their metrics, incl.
16 = withdraw) is routing policy, like DNS/DHCP. So with **no operator policy** (no server
instruction and no per-event handler), the server applies the spec-safe **static default of
advertising nothing** and answers with **no LLM round-trip** (gated by `operator_wants_dynamic` in
`mod.rs` = `has_instruction || has_handler`). There is no mechanical RIP reply to synthesise —
RIP has no Hello/keepalive machinery. The LLM is consulted **only when the operator opts in** with
the routing policy (an instruction or a handler); the LLM-driven route material below describes
that opt-in path.

**Status**: Experimental. Nothing has ever pointed a real RIP implementation at this server —
the tests build RIP datagrams by hand from RFC 2453, which is an independent *reading* of the
spec, not an independent implementation. That is the `dhcp` situation, and it is the reason this
is not Beta.

**Protocol spec**: [RFC 2453 (RIPv2)](https://datatracker.ietf.org/doc/html/rfc2453),
[RFC 1058 (RIPv1)](https://datatracker.ietf.org/doc/html/rfc1058)

**Port**: UDP 520. `PrivilegeRequirement::PrivilegedPort(520)` is declared and is live rather
than decorative — 520 really is below 1024. It does not obstruct testing: `server_startup.rs`
only enforces a `PrivilegedPort` when `port > 0 && port < 1024`, so the suite's ephemeral ports
run the whole event -> LLM -> action path unprivileged.

## Library Choices

### No Library - Manual Protocol Implementation

**Why no library**:

- No mature Rust RIP server library exists
- Available crates focus on routing table manipulation, not RIP protocol
- Holo routing suite includes RIP but is too heavy (full YANG models, gRPC interfaces)
- RIP protocol is simple and well-specified in RFC 2453
- Manual implementation provides full LLM control over routing decisions

**What we implement manually**:

- RIP packet construction (Request and Response messages)
- Route entry parsing (AFI, IP, subnet mask, next hop, metric)
- Message header parsing (command, version)
- Metric handling (1-15 = reachable, 16 = unreachable)

**Why not alternatives**:

- `net-route` - Routing table manipulation, not RIP protocol
- `holo` - Full routing suite with YANG/gRPC, too heavyweight
- External RIP daemon (Quagga, FRR) - Violates NetGet architecture

## Architecture Decisions

### UDP-Based Protocol

RIP uses UDP for lightweight message exchange:

```rust
let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
```

Each message is independent (stateless), similar to NTP.

### RIP Packet Structure

RIP packets consist of a 4-byte header followed by up to 25 route entries:

**Header (4 bytes)**:

```
+--------+--------+--------+--------+
|Command | Version|     Unused      |
+--------+--------+--------+--------+
```

- Command: 1 = Request, 2 = Response
- Version: 2 (for RIPv2)
- Unused: Must be zero

**Route Entry (20 bytes each, max 25 per packet)**:

```
+---------+---------+---------+---------+
|    AFI (2 bytes)  | Route Tag (2 bytes)|
+---------+---------+---------+---------+
|        IP Address (4 bytes)           |
+---------+---------+---------+---------+
|       Subnet Mask (4 bytes)           |
+---------+---------+---------+---------+
|        Next Hop (4 bytes)             |
+---------+---------+---------+---------+
|         Metric (4 bytes)              |
+---------+---------+---------+---------+
```

- AFI (Address Family Identifier): 2 for IPv4
- Route Tag: Used for external route tagging (typically 0)
- IP Address: Network address
- Subnet Mask: Subnet mask for the route (RIPv2 only, classless)
- Next Hop: Next hop router address (0.0.0.0 = use sender's address)
- Metric: Hop count (1-15 reachable, 16 = unreachable/infinity)

### Message Types

**Request (Command = 1)**:

- Query for routing information
- Can request entire table (AFI=0, metric=16) or specific routes
- Typically sent on startup or when triggered

**Response (Command = 2)**:

- Advertise routing table entries
- Sent in response to requests or periodically (every 30 seconds in production)
- Contains up to 25 route entries per packet

### Metric and Hop Count

RIP uses hop count as its metric:

- 1-15: Valid metric (number of hops to destination)
- 16: Infinity (unreachable route)
- Maximum 15 hops prevents routing loops

### Stateless Processing

Each RIP message is processed independently:

- No session state (unlike BGP)
- No connection tracking (unlike TCP)
- Each datagram creates a new "connection" for UI display
- No persistent peer relationships

## LLM Integration

### Startup Parameters

Server configured with routing table:

```json
{
  "routes": [
    {"ip": "192.168.1.0", "subnet": "255.255.255.0", "metric": 1},
    {"ip": "10.0.0.0", "subnet": "255.0.0.0", "metric": 5}
  ]
}
```

Extracted from LLM-generated startup prompt.

### Sync Actions (Network event triggered)

1. **send_rip_response**: Send routing table response
    - Routes: Array of route entries (ip_address, subnet_mask, next_hop, metric)
    - Max 25 routes per packet

2. **send_rip_request**: Request routing information
    - Optional routes parameter for specific queries
    - Omit routes to request entire table

3. **ignore_request**: Don't send any response

### Event Types

- `rip_request`: Triggered when any RIP message is received (request or response)
    - Parameters: command, version, message_type, routes, peer_address, bytes_received

**Event data example**:

```json
{
  "event": "rip_request",
  "data": {
    "command": 1,
    "version": 2,
    "message_type": "request",
    "routes": [
      {
        "afi": 2,
        "route_tag": 0,
        "ip_address": "0.0.0.0",
        "subnet_mask": "0.0.0.0",
        "next_hop": "0.0.0.0",
        "metric": 16
      }
    ],
    "peer_address": "192.0.2.10:520",
    "bytes_received": 24
  }
}
```

## Connection Management

### Pseudo-Connection Lifecycle

1. **Receive**: UDP datagram on port 520
2. **Parse**: Extract command, version, and route entries
3. **Register**: Create ConnectionId and add to ServerInstance
4. **Process**:
   - **Default (no operator policy)** → static default: advertise nothing / stay silent, **no LLM call**
   - **Opt-in only** (instruction or handler) → call the handler/LLM with the `rip_request` event
5. **Build**: Construct RIP response packet (opt-in path)
6. **Respond**: Send UDP response
7. **Track**: Connection remains in UI

### Connection Data Structure

```rust
ProtocolConnectionInfo::Rip {
    recent_peers: Vec<(SocketAddr, Instant)>, // Track recent peer activity
}
```

No persistent connections - each message is independent.

### Backend failure: silence, deliberately (not the "goes quiet on LLM error" defect)

RFC 2453 defines exactly two commands — Request (1) and Response (2). There is **no error
message, no NAK, no status code**. The only thing this server could put on the wire after an
LLM failure is a Response, and a Response is a *routing assertion*: an empty one claims "I have
no routes", a metric-16 one poisons the destination in the peer's table. Both are fabricated
policy that netget has no basis for when the backend that holds the policy is unreachable.

So the LLM-error branch stays silent — the same static default this server already applies when
no routing policy is configured. This is safe rather than a hang, because RIP is connectionless
and unsolicited: a peer that gets no Response is not blocked on a socket, it re-requests on its
own timer and otherwise relies on the 30-second periodic updates. Nothing is interpolated onto
the wire, so there is nothing for `WireFailure` to render here.

The distinction that would be carried by protocol error codes elsewhere is carried in the log
instead. Grep `decision=` in `mod.rs`:

| tag | meaning |
|---|---|
| `decision=static_default_silent` | no operator policy; no LLM call was made |
| `decision=model_advertise` | the model produced route actions, which were sent |
| `decision=model_ignore` | the model explicitly chose `ignore_request` — a routing decision |
| `decision=no_answer_silent` | the model was asked and returned no actions |
| `decision=fail_closed_llm_error` | the LLM call errored; also logs `category=overloaded` vs `category=unavailable` (`WireFailure::classify`) and the full error |

Only the last is a fault. Keeping it apart from `model_ignore` is the point: a deliberate
"advertise nothing" and a backend outage look identical on the wire and must not look identical
in the log.

## Limitations

### Partial Implementation

**Implemented**:

- ✅ RIPv2 packet parsing and construction
- ✅ Request and Response messages
- ✅ Route entry parsing (AFI, IP, subnet, next hop, metric)
- ✅ Basic routing table advertisement
- ✅ Message validation

**Not Implemented**:

- ❌ Routing table storage (LLM provides routes on-demand)
- ❌ Periodic updates (no 30-second timer, only responds to requests)
- ❌ Route timeout/garbage collection
- ❌ Triggered updates (route changes)
- ❌ Split horizon and poison reverse (loop prevention)
- ❌ Authentication (RFC 2082)
- ❌ RIPv1 compatibility mode
- ❌ Multicast support (224.0.0.9)
- ❌ Route filtering/access lists
- ❌ Route aggregation

### No Routing Table

Server doesn't maintain routing table:

- LLM generates routes on-demand for each request
- Cannot learn routes from other RIP speakers
- Cannot propagate learned routes
- No best path selection

### No Loop Prevention

Standard RIP loop prevention not implemented:

- No split horizon (don't advertise routes back to source)
- No poison reverse (advertise unreachable routes to source)
- No hold-down timers
- No route timeout/expiration

### Testing Limitations

RIP protocol requires multiple routers:

- E2E tests cover basic request/response
- No tests for route convergence or loop prevention
- No tests for triggered updates or periodic updates

**No third-party RIP implementation has ever been pointed at this server.** The peers in
`tests/server/rip/` are RIP datagrams assembled by hand from RFC 2453 inside the test itself —
an independent reading of the spec, not an independent implementation, which is the same
footing `dhcp` is on and why the rating stays Experimental. Promoting it means driving it with
something like FRRouting's `ripd`, not adding more hand-built datagrams.

`tests/server/rip/action_validation_test.rs` covers the executor's own contract instead: that a
metric survives to the wire unchanged across 1-16, that a value outside that range or a
`route_tag` too large for its 2-byte slot is refused rather than truncated, and that both
`send_rip_response` and `send_rip_request` hold the same 25-entry ceiling.

### No Production Use

**DO NOT use for production routing**:

- No routing table persistence
- No route learning or propagation
- No loop prevention mechanisms
- No periodic updates or route expiration

For production routing, use established implementations (FRR, Quagga, BIRD).

## Examples

### Server Startup

```
netget> Listen on port 520 via RIP. Advertise routes for 192.168.1.0/24 (metric 1) and 10.0.0.0/8 (metric 5).
```

Server output:

```
[INFO] RIP server listening on 0.0.0.0:520
→ RIP server ready on 0.0.0.0:520
```

### Request Entire Routing Table

Client sends request (AFI=0, metric=16):

```
[DEBUG] RIP received 24 bytes from 192.0.2.10:520 (cmd=1, ver=2, entries=1)
[TRACE] RIP data (hex): 010200000000000000000000000000000000000000000010
```

LLM receives event:

```json
{
  "event": "rip_request",
  "data": {
    "command": 1,
    "message_type": "request",
    "routes": [{"afi": 0, "metric": 16}]
  }
}
```

LLM responds with routes:

```json
{
  "actions": [
    {
      "type": "send_rip_response",
      "routes": [
        {
          "ip_address": "192.168.1.0",
          "subnet_mask": "255.255.255.0",
          "next_hop": "0.0.0.0",
          "metric": 1
        },
        {
          "ip_address": "10.0.0.0",
          "subnet_mask": "255.0.0.0",
          "next_hop": "0.0.0.0",
          "metric": 5
        }
      ]
    }
  ]
}
```

Server sends response:

```
[DEBUG] RIP sent 44 bytes to 192.0.2.10:520
→ RIP response to 192.0.2.10:520 (44 bytes)
```

### Receiving Route Advertisement

Peer sends response with routes:

```
[DEBUG] RIP received 44 bytes from 192.0.2.10:520 (cmd=2, ver=2, entries=2)
```

LLM receives event:

```json
{
  "event": "rip_request",
  "data": {
    "command": 2,
    "message_type": "response",
    "routes": [
      {
        "ip_address": "172.16.0.0",
        "subnet_mask": "255.255.0.0",
        "next_hop": "192.0.2.10",
        "metric": 3
      },
      {
        "ip_address": "10.20.0.0",
        "subnet_mask": "255.255.0.0",
        "next_hop": "192.0.2.10",
        "metric": 8
      }
    ]
  }
}
```

LLM can log, store, or respond to received routes.

### Advertising Unreachable Route

LLM responds with metric 16 (infinity):

```json
{
  "type": "send_rip_response",
  "routes": [
    {
      "ip_address": "192.168.99.0",
      "subnet_mask": "255.255.255.0",
      "metric": 16
    }
  ]
}
```

This indicates the route is unreachable (withdrawn).

## Use Cases

### Learning RIP Protocol

- Understand RIP message format and routing updates
- Experiment with distance-vector routing
- Practice RIP configuration

### Honeypot/Monitoring

- Detect unauthorized RIP speakers on network
- Log RIP reconnaissance attempts
- Monitor for routing attacks

### Testing

- Test RIP client implementations
- Validate RIP message parsing
- Simulate router behavior

### Route Injection Testing

- Inject controlled routes for testing
- Simulate route changes
- Test route filtering and policy

### NOT for Production Routing

RIP server should **not** be used for production routing:

- No routing table or route learning
- No loop prevention mechanisms
- No periodic updates or route expiration
- No authentication or security

For production routing, use FRR, Quagga, or BIRD.

## Performance Characteristics

### Latency

- **With Scripting**: Sub-millisecond (script handles requests)
- **Without Scripting**: 2-5 seconds (one LLM call per message)
- Packet construction: ~10-20 microseconds

### Throughput

- **With Scripting**: Thousands of messages per second
- **Without Scripting**: Limited by LLM (~0.2-0.5 messages/sec)
- RIP traffic is typically low volume

### Scripting Compatibility

RIP is excellent for scripting:

- Simple request/response logic
- Deterministic responses
- No complex state machine

When scripting enabled:

- Server startup generates script (1 LLM call)
- All requests handled by script (0 LLM calls)
- Script can generate routing table instantly

## References

- [RFC 2453: RIPv2](https://datatracker.ietf.org/doc/html/rfc2453)
- [RFC 1058: RIPv1](https://datatracker.ietf.org/doc/html/rfc1058)
- [RFC 2082: RIP-2 MD5 Authentication](https://datatracker.ietf.org/doc/html/rfc2082)
- [RIP on Wikipedia](https://en.wikipedia.org/wiki/Routing_Information_Protocol)
- [Holo Routing Suite](https://github.com/holo-routing/holo)
