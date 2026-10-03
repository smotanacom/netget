# ICMP Client Implementation

## Overview

The ICMP (Internet Control Message Protocol) client implementation provides network-layer diagnostic capabilities, primarily for ping and traceroute functionality. The client sends ICMP Echo Requests and processes replies with full LLM control.

## Library Choices

### Raw Socket Implementation
- **socket2** (v0.5) - Raw IP socket creation
  - Required for ICMP protocol access (IP protocol 1)
  - Requires `CAP_NET_RAW` capability or root access
  - Non-blocking I/O for async integration

### Packet Handling
- **pnet_packet** (v0.35) - ICMP packet construction and parsing
  - Echo Request/Reply packet types
  - Destination Unreachable parsing
  - Time Exceeded parsing (for traceroute)
  - Automatic checksum calculation

### Async Runtime
- **tokio** - Async runtime integration
  - Async receive loop
  - LLM integration
  - State machine management

## Architecture

### Protocol Pattern
Follows the pattern of **UDP client** and **TCP client**:
1. Raw ICMP socket creation
2. Send Echo Requests based on LLM actions
3. Receive loop processing replies
4. State machine prevents concurrent LLM calls
5. RTT (Round-Trip Time) measurement

### Connection Model
**Connectionless** - Like UDP:
- No persistent connection
- Each request/reply pair is independent
- Pending request tracking for RTT calculation
- Timeout handling for lost packets

### Socket Configuration
```rust
Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))
```
- Receives ICMP replies destined for this host
- Requires elevated privileges (`CAP_NET_RAW` or root)
- Non-blocking mode with async polling
- **`IP_HDRINCL` is set.** `build_echo_request` emits a complete IPv4 packet, header included.
  Without the option the kernel prepends a header of its own and our 20 bytes become the front
  of the ICMP *message*, so the target reads type 0x45 (69, unassigned) and no ping this client
  ever sent could have been answered. It is also what makes the advertised `ttl` parameter mean
  anything: with the kernel building the header, TTL is the kernel's to choose and traceroute
  is impossible. `IcmpClient::apply_action` calls
  `crate::server::icmp::prepare_ipv4_for_raw_send` immediately before `send_to`, which converts
  `ip_len`/`ip_off` to host byte order on Darwin and FreeBSD and is a no-op on Linux — see the
  server's CLAUDE.md for why that conversion exists. **None of this has been executed against a
  kernel**; opening the socket needs root.
- Source address is left as `0.0.0.0` so the kernel fills in the outgoing interface's address.

### Packet Flow
1. **LLM Decision**: LLM returns `send_echo_request` action
2. **Build**: Construct ICMP Echo Request with IP header
3. **Send**: Send via raw socket to destination
4. **Track**: Store pending request with timestamp
5. **Receive**: Async loop receives ICMP replies
6. **Match**: Match reply to pending request (identifier + sequence)
7. **Calculate RTT**: Measure time delta
8. **LLM Event**: Call LLM with echo_reply event
9. **Execute**: LLM decides next action (send more, disconnect, etc.)

## LLM Integration

### Events

#### ICMP Connected
```json
{
  "event_type": "icmp_connected",
  "local_addr": "0.0.0.0:0",
  "target_ip": "8.8.8.8"
}
```

#### ICMP Echo Reply (Ping Response)
```json
{
  "event_type": "icmp_echo_reply",
  "source_ip": "8.8.8.8",
  "identifier": 1234,
  "sequence": 1,
  "rtt_ms": 15,
  "ttl": 56,
  "payload_hex": "48656c6c6f"
}
```

#### ICMP Timeout
```json
{
  "event_type": "icmp_timeout",
  "destination_ip": "192.168.1.100",
  "identifier": 1234,
  "sequence": 1
}
```

#### ICMP Destination Unreachable
```json
{
  "event_type": "icmp_destination_unreachable",
  "source_ip": "192.168.1.1",
  "code": 1  // 0=net, 1=host, 2=protocol, 3=port
}
```

#### ICMP Time Exceeded (Traceroute)
```json
{
  "event_type": "icmp_time_exceeded",
  "source_ip": "10.0.0.1",  // Hop address
  "code": 0  // 0=TTL exceeded, 1=fragment reassembly
}
```

### Actions

#### Send Echo Request (Ping)
```json
{
  "type": "send_echo_request",
  "destination_ip": "8.8.8.8",
  "identifier": 1234,
  "sequence": 1,
  "payload_hex": "48656c6c6f",
  "ttl": 64
}
```

There is **no** `send_timestamp_request`. The action, its executor arm and the client-side
parsing are all present in the source and all commented out: pnet 0.35 ships no `timestamp` /
`timestamp_reply` packet types. Do not document or promise it. (This section used to show it
as if it worked.)

#### Wait for More Responses
```json
{
  "type": "wait_for_more"
}
```

#### Disconnect
```json
{
  "type": "disconnect"
}
```

## Use Cases

### Basic Ping
```
"Ping 8.8.8.8 five times and report average latency"
```

LLM logic:
1. Send 5 echo requests (seq 1-5)
2. Receive replies, calculate RTT for each
3. Average the RTT values
4. Report results
5. Disconnect

### Traceroute
```
"Perform traceroute to example.com"
```

LLM logic:
1. Send echo request with TTL=1
2. Receive Time Exceeded from first hop
3. Send echo request with TTL=2
4. Receive Time Exceeded from second hop
5. Continue until Echo Reply (destination reached)
6. Report path

### Latency Monitoring
```
"Ping 1.1.1.1 every second and alert if latency > 100ms"
```

LLM logic:
1. Send echo request
2. Receive reply, check RTT
3. If RTT > 100ms, alert user
4. Wait 1 second
5. Repeat

## Pending Request Tracking

### Transaction ID
ICMP uses `(identifier, sequence)` tuple to match requests to replies:
- **Identifier**: Usually process ID or random value
- **Sequence**: Incrementing counter per ping session

### RTT Calculation
```rust
struct PendingRequest {
    sent_at: Instant,
    identifier: u16,
    sequence: u16,
    destination_ip: Ipv4Addr,
}

// When reply arrives:
let rtt_ms = req.sent_at.elapsed().as_millis();
```

### Timeout Handling
Implemented, and no longer a TODO. The receive loop sweeps the pending map on every idle
(`WouldBlock`) pass: any request older than `ICMP_REPLY_TIMEOUT_SECS` (5) is removed and raises
`icmp_timeout` with `waited_ms`, so the model is told about the one outcome that matters most
for a reachability check. A ping that gets no reply *is* the result.

One edge worth knowing: the sweep only runs on the idle branch, so under a continuous inbound
stream a timeout is noticed late. That is the right trade for a diagnostic client and is not a
correctness problem, but do not describe the 5 seconds as a hard deadline.

## Limitations

### Privilege Requirements
**CRITICAL**: Requires root or `CAP_NET_RAW` capability
- Cannot run in unprivileged environments
- Not available in Claude Code for Web (sandboxed)
- Testing requires elevated permissions

### Platform Considerations
- **Linux**: root or `CAP_NET_RAW` (`setcap cap_net_raw+ep ./netget`)
- **macOS**: root only — no capabilities model, and this uses `SOCK_RAW` rather than the
  unprivileged `SOCK_DGRAM`/`IPPROTO_ICMP` path `ping(8)` uses
- **Windows**: not shipped — `icmp` is excluded from the `dist-windows` feature set

### Kernel Interaction
- Kernel may intercept ICMP Echo Replies before userspace
- Some ICMP types filtered by firewall
- May need to configure system to allow raw ICMP

### IPv6 Support
Not yet implemented:
- Current implementation only supports ICMPv4
- ICMPv6 uses different socket (Protocol::ICMPV6)
- ICMPv6 message types differ

### Timestamp Requests
Not implemented at all — the action is not even advertised. See the note in the Actions
section: pnet has no timestamp packet types.

### Numeric parameters are range-checked
`identifier` and `sequence` are 16-bit, `ttl` is 8-bit. All three are optional with documented
defaults, but **present-but-invalid is now an error rather than a silent fallback**:
`as_u64().unwrap_or(1234)` turned `"identifier": "5678"` — a string, which a model produces
readily — into 1234 without a word, and the reply could then never be matched to the request.

## There is no state machine, and there does not need to be

This section used to describe an `Idle`/`Processing`/`Accumulating` cycle that "prevents
concurrent LLM calls on same client". The enum existed, two of its three variants carried
`#[allow(dead_code)]`, and the field was written once at construction and never read or
transitioned. It has been deleted rather than wired up: what actually serialises this client is
that its single receive loop awaits each `call_llm_for_client` inline, so a second event cannot
be in flight. `ClientData` is now just the model's memory.

**The memory is what the concurrency bug was actually about.** `call_llm_for_client(…,
&client_data.lock().await.memory, …)` written inline in a `match` scrutinee keeps the guard
alive for the whole `match`, and the `Ok` arm then locks again to store `memory_updates` —
against a `tokio::sync::Mutex`, which is not reentrant. That is a permanent hang, and it fired
on every successful call that returned memory, in all three places the client calls the model
(connect, echo reply, timeout). One of the three even wrote `.memory.clone()`, which copies the
string and does nothing about the lock. The fix is to snapshot the memory into a local and drop
the guard *before* the call; the root CLAUDE.md states the rule as "never hold a guard across
an `.await`", and a `match` scrutinee is the shape that hides it.

## Performance

### Latency Breakdown
- Packet construction: < 1ms
- Raw socket send: < 1ms
- Network RTT: 1-500ms (network dependent)
- Packet parsing: < 0.1ms
- LLM call: 100-500ms (dominant factor for next action)

### Throughput
- Can send multiple pings in quick succession
- Limited by LLM response time for adaptive logic
- Scripting mode could bypass LLM for predictable patterns

## Security Considerations

### Legitimate Uses
- Network diagnostics (ping, traceroute)
- Latency monitoring
- Reachability testing
- Network path discovery

### Potential Misuse
- ⚠️ ICMP flood attacks (DoS)
- ⚠️ Network scanning without authorization
- ⚠️ Covert channels

### Safeguards
- No default flooding behavior
- LLM controls send rate
- Requires explicit root access
- Documentation emphasizes authorized use only

## Example Prompts

### Simple Ping
```
"Ping 8.8.8.8 three times"
```

### Latency Test
```
"Ping cloudflare DNS (1.1.1.1) and Google DNS (8.8.8.8), compare latency"
```

### Traceroute
```
"Trace route to github.com by incrementing TTL"
```

### Conditional Logic
```
"Ping 192.168.1.1 until it responds, then alert me"
```

## Testing Notes

See `tests/client/icmp/CLAUDE.md` for test strategy and E2E test details.

**What is actually proven** (`tests/client/icmp/action_codec_test.rs`, unprivileged): the echo
request's bytes, field by field at their RFC 791 / RFC 792 offsets with both checksums
verified; that every advertised example is accepted by its own executor; the documented
defaults; and that eight shapes of malformed action are refused rather than silently
defaulted. `command_channel_test.rs` proves the unprivileged failure contract. **No test has
ever sent an ICMP packet** — everything past `send_to` is unverified.

## Future Enhancements

1. **Timeout Timer**: Automatic timeout for pending requests
2. **IPv6 Support**: ICMPv6 with Neighbor Discovery
3. **Timestamp Requests**: Full implementation
4. **Bulk Ping**: Send multiple requests concurrently
5. **Statistics**: Min/max/avg/stddev RTT calculation
6. **Packet Loss**: Track sent vs received ratio

## References

- RFC 792 - Internet Control Message Protocol (ICMP)
- RFC 1122 - Requirements for Internet Hosts
- pnet documentation: https://docs.rs/pnet/latest/pnet/
- socket2 documentation: https://docs.rs/socket2/latest/socket2/

## Injected commands (the dashboard's `[ send ]`)

The client registers a command channel (`command_support::register_command_channel`) as soon as
the raw socket exists, and spawns the task that drains it **before** the connected-event LLM
call, which is awaited inline in `connect_with_llm_actions` and which a manual `*` routing rule
parks. Commands are drained by their own registered task rather than a `tokio::select!` arm:
the receive loop awaits `call_llm_for_client` inline, so a `select!` arm there would stall for a
whole LLM round-trip.

Injected actions go through `IcmpClient::apply_action`, the same function `execute_actions`
(the LLM path) now loops over, so the echo-request encoding exists exactly once. Outcomes:

| Injected action | `ClientSendOutcome` |
|---|---|
| `send_echo_request` | `Sent { bytes_sent }` — the byte count `Socket::send_to` returned (20-byte IP header + 8-byte ICMP header + payload). The pending-request map is updated exactly as on the LLM path, so an injected ping still gets its RTT |
| `wait_for_more` | `Executed { detail: "wait_for_more" }` |
| `disconnect` | `Disconnected` — aborts the receive loop, drops the handle, marks the client Disconnected. ICMP has no wire close |
| unknown action | `Rejected { error }` |

**Privilege.** Everything above needs the raw socket, which needs root/`CAP_NET_RAW`. Without
it `connect()` fails before the channel is ever registered, and the contract that failure has
to honour is rule 3: **no command handle is left behind**, so the dashboard greys `[ send ]`
out and a late `send_to_client` fails fast instead of hanging. The connected-event error path
now removes the handle explicitly before its early `return Err(…)`.
`tests/client/icmp/command_channel_test.rs` asserts that unprivileged behaviour on every run
and `#[ignore]`s only the test that actually puts a packet on the wire.

The receive loop is a real tokio task (non-blocking `recv_from` plus a 10 ms sleep), not the
fire-and-forget `spawn_blocking` shape the server side had — the command task is genuinely
alive alongside it.
