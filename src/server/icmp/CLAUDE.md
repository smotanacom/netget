# ICMP Server Implementation

## Overview

The ICMP (Internet Control Message Protocol) server implementation provides a network-layer protocol server that can capture and respond to ICMP messages. ICMP is primarily used for network diagnostics, error reporting, and control messages.

## Library Choices

### Raw Socket Implementation
- **socket2** (v0.5) - Raw IP socket creation and management
  - Provides cross-platform raw socket support
  - Required for ICMP protocol access (IP protocol 1)
  - Requires `CAP_NET_RAW` capability or root access

### Packet Handling
- **pnet_packet** (v0.35) - ICMP packet parsing and construction
  - Comprehensive ICMP packet types (Echo, Destination Unreachable, Time Exceeded, Timestamp, etc.)
  - Automatic checksum calculation
  - Type-safe packet builders and parsers
  - Already used in ARP and DataLink protocols

### Async Runtime
- **tokio** - Async runtime integration
  - `tokio::task::spawn_blocking` for raw socket operations
  - Async LLM integration while maintaining blocking I/O

## Architecture

### Protocol Pattern
Follows the same pattern as **ARP server** (`src/server/arp/mod.rs`):
1. Raw socket creation in blocking context
2. Receive loop with packet parsing
3. LLM integration via async tasks
4. Separate send socket to avoid conflicts

### Connection Model
**Connectionless** - Like UDP and ARP:
- No persistent connections
- Each ICMP message triggers LLM independently
- No state machine per connection
- **No connection tracking at all.** This server never calls `add_connection` or
  `update_connection_stats`, so the dashboard shows it with no peers and no `↓/↑` counters.
  That is a gap, not a design: an operator watching an ICMP honeypot sees nothing but the log.
  `metadata()` declares `.connectionless()` so that if per-remote entries are ever added, the
  10-second idle sweep reaps them (see the root CLAUDE.md, "The 10-second idle sweep"). An
  earlier version of this file claimed "'Connection' tracking is per-packet for TUI display";
  there was none.

### Socket Configuration
```rust
Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))
```
- Receives all ICMP packets destined for this host
- Requires elevated privileges (`CAP_NET_RAW` or root)
- Non-blocking mode, polled with a 10 ms sleep, so `StopSignal` ends the loop within ~10 ms

### `IP_HDRINCL` — the thing that makes the send path mean anything

`build_echo_reply` and its siblings return a **complete IPv4 packet**, header included. A
`SOCK_RAW`/`IPPROTO_ICMP` socket does not know that unless it is told: with `IP_HDRINCL` off
the kernel builds its own header and treats everything handed to `send_to` as the ICMP
*message*, so our 20-byte header lands where the ICMP type byte belongs and the peer reads
type 0x45 (69, unassigned). **Every reply this server produced before August 2026 was that.**
The send socket now sets `set_header_included_v4(true)`; pnet's own Layer3 transport channel
sets the same option for the same reason.

Two consequences worth carrying:

- **Darwin and FreeBSD want `ip_len` and `ip_off` in host byte order** on such a socket. XNU's
  `rip_output` compares `ip->ip_len` against the real mbuf length, so a network-order 28 reads
  as 7168 and the send fails with `EINVAL`. `prepare_ipv4_for_raw_send` (in `mod.rs`) does that
  conversion immediately before `send_to` and zeroes the now-stale header checksum for the
  kernel to refill. It is a no-op on Linux. This mirrors `pnet_transport::send_to_impl`.
  **After that call the buffer is no longer a well-formed RFC 791 header — nothing may parse
  it again.**
- The receive path still tolerates **IP-in-IP**: a peer that made the same mistake sends us
  `kernel header | its header | ICMP`. That is what the "IP-in-IP encapsulation on loopback"
  branch in the receive loop unwraps, and loopback had nothing to do with it — the source of
  that traffic was this repo's own `#[ignore]`d e2e test, which had the identical bug and has
  been fixed too.

None of this has been executed. Opening the socket needs root; see "Maturity" below.

### Packet Flow
1. **Receive**: Raw socket receives IP packets
2. **Parse**: Extract IPv4 header, validate ICMP protocol
3. **Identify**: Determine ICMP message type (Echo, Timestamp, etc.)
4. **Event**: Build appropriate event with structured data
5. **LLM**: Call LLM with event, get actions
6. **Execute**: Construct and send ICMP reply packets
7. **Send**: Use separate raw socket to send responses

## LLM Integration

### Events

#### ICMP Echo Request (Ping)
```json
{
  "event_type": "icmp_echo_request",
  "source_ip": "192.168.1.50",
  "destination_ip": "192.168.1.100",
  "identifier": 1234,
  "sequence": 1,
  "payload_hex": "48656c6c6f",
  "ttl": 64
}
```

#### Other ICMP Messages
```json
{
  "event_type": "icmp_other_message",
  "source_ip": "192.168.1.50",
  "destination_ip": "192.168.1.100",
  "icmp_type": 3,
  "icmp_code": 1,
  "packet_hex": "..."
}
```

### Actions

#### Send Echo Reply
```json
{
  "type": "send_echo_reply",
  "source_ip": "192.168.1.100",
  "destination_ip": "192.168.1.50",
  "identifier": 1234,
  "sequence": 1,
  "payload_hex": "48656c6c6f"
}
```

#### Send Destination Unreachable
```json
{
  "type": "send_destination_unreachable",
  "source_ip": "192.168.1.1",
  "destination_ip": "192.168.1.50",
  "code": 1,
  "original_packet_hex": "4500001c1c460000401160abc0a80132cb007105a1120035000860b6"
}
```
`original_packet_hex` is the RFC 792 quotation: the offending IPv4 header plus the next 64
bits, and it is **decoded, not passed through** — the executor `hex::decode`s it, refuses
non-hex, and the builder truncates to 28 bytes (and does not panic on fewer). Nothing is
elided in the advertised example; a model that copies it emits a datagram a peer can match.

#### Send Time Exceeded (Traceroute)
```json
{
  "type": "send_time_exceeded",
  "source_ip": "10.0.0.1",
  "destination_ip": "192.168.1.50",
  "code": 0,
  "original_packet_hex": "4500001c1c47000001119faac0a80132cb007105a112829a0008de50"
}
```
`code` is the only optional numeric field, defaulting to 0 (TTL exceeded in transit).

#### Ignore ICMP
```json
{
  "type": "ignore_icmp"
}
```

### Numeric fields are range-checked, not truncated

`identifier` and `sequence` are 16-bit; `code` is 8-bit. Reading them with `as u64(...) as u16`
truncates in silence — an identifier of 70000 becomes 4464 and the reply matches no request the
peer ever sent. `u16_field` / `u8_field` in `actions.rs` refuse out-of-range and non-numeric
values with an error naming the field and the range, which is something the model can act on.

The **events** hand these back as JSON numbers, matching the `number` type hint the event
declares and the `as_u64()` the executor uses. They used to be stringified
(`identifier.to_string()`), which broke this protocol's own scripted startup example: it copies
`event["identifier"]` straight into the action, and the action was refused with "Missing
'identifier' parameter".

## ICMP Message Types Supported

### Implemented (server can construct and send)
- **Echo Request/Reply (Type 8/0)** - Ping functionality
- **Destination Unreachable (Type 3)** - Error reporting (codes: net, host, protocol, port, etc.)
- **Time Exceeded (Type 11)** - TTL expiry (used in traceroute)

### NOT implemented
- **Timestamp Request/Reply (Type 13/14)** - the `icmp_timestamp_request` event, the
  `send_timestamp_reply` action and `IcmpServer::build_timestamp_reply()` are all present in the
  source but **commented out** (`src/server/icmp/mod.rs`, `src/server/icmp/actions.rs`) because
  `pnet` 0.35 ships no `timestamp` / `timestamp_reply` packet types. A Timestamp request therefore
  arrives as a generic `icmp_other_message` event with `icmp_type: 13`, and there is no action that
  can answer it. Do not document or promise timestamp support until pnet gains those types (or the
  20-byte message is built by hand).

### Observable only (surface as `icmp_other_message`, no reply action)
- **Source Quench (Type 4)**
- **Redirect (Type 5)**
- **Timestamp Request/Reply (Type 13/14)**
- **Parameter Problem (Type 12)**
- **Address Mask Request/Reply (Type 17/18)**
- **Router Advertisement/Solicitation**

## Packet Construction

### IP Header
All ICMP packets are wrapped in IPv4 headers:
- Version: 4
- Header Length: 5 (20 bytes)
- TTL: 64
- Protocol: ICMP (1)
- Source/Destination from action parameters
- Checksum automatically calculated

### ICMP Checksum
Calculated using `pnet::packet::icmp::checksum()`:
- Covers entire ICMP packet (header + payload)
- Uses ones' complement sum algorithm
- Essential for packet validity

## Maturity: `Experimental`, precisely

**Proven**, unprivileged, by `tests/server/icmp/packet_codec_test.rs`:

* every packet the server can build, field by field at its RFC 791 / RFC 792 offset, with both
  checksums *verified* rather than compared to a constant;
* that a short or oversized quoted datagram is handled rather than panicking;
* that every advertised action example is accepted by its own executor;
* that thirteen shapes of hostile action JSON — non-hex payloads, out-of-range identifiers, a
  stringified number, an IPv6 address, a payload larger than a datagram — are **refused**, not
  truncated and not panicking;
* what `prepare_ipv4_for_raw_send` does on the platform being compiled for.

**Not proven, and not to be claimed:**

* **the raw socket transport, which has never been executed.** Nothing in this repo has opened
  `SOCK_RAW`/`IPPROTO_ICMP` outside an `#[ignore]`d test, so no reply has been observed leaving
  a socket, and the `IP_HDRINCL` fix above is reasoned from kernel sources and pnet's
  long-standing workaround rather than measured.
* the Darwin/FreeBSD host-byte-order conversion, against a real kernel.
* the `interface` argument, which is accepted and ignored (below).
* received `total_length` on BSD, which arrives in host byte order; pnet clamps the payload
  slice to the buffer, so this degrades to "use everything captured" rather than a wrong slice.

**Path to Beta**: run `tests/server/icmp/e2e_test.rs` under `sudo` against a real independent
client — `ping(8)` is the obvious one — and assert the reply. Nothing less counts.

## Limitations

### Privilege Requirements
**CRITICAL**: Requires root or `CAP_NET_RAW` capability
- Cannot run in unprivileged environments
- Not available in Claude Code for Web (sandboxed)
- Testing requires elevated permissions

### Platform Considerations
`socket2`'s `Domain::IPV4` / `Type::RAW` / `Protocol::ICMPV4` maps to the POSIX
`socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` on every Unix, so there is no per-OS code path here.

- **Linux**: works as root or with `CAP_NET_RAW` (`setcap cap_net_raw+ep ./netget`).
- **macOS**: works **only as root** - macOS has no capabilities model, and unlike ping(8) this
  server uses `SOCK_RAW` rather than the unprivileged `SOCK_DGRAM`/`IPPROTO_ICMP` path. Note also
  that the macOS kernel answers echo requests itself, so a userspace reply is a *second* reply on
  the wire; test against a target that has `net.inet.icmp.bmcastecho`-style kernel handling in mind.
- **Windows**: not shipped - `icmp` is excluded from the `dist-windows` feature set.

### Startup failure is loud
Raw-socket creation happens inside a `spawn_blocking` task, but its result is handed back over a
`oneshot` before `spawn_with_llm` returns. A privilege failure therefore propagates out of
`Server::spawn()`, `server_startup` records `ServerStatus::Error(..)`, and an MCP caller sees:

```
Failed to start server: failed to create raw ICMP receive socket (needs root, or CAP_NET_RAW on Linux)
```

The server is never reported as `Running` when the socket did not open.

Guarded by `tests/capture_startup_reports_failure_test.rs`
(`icmp_spawn_outcome_matches_raw_socket_privilege`), which asserts both branches: privileged
`spawn` returns `Ok`, unprivileged `spawn` returns `Err` naming the raw socket. The unprivileged
branch is the one that runs on every developer machine and in CI, so a regression to the old
fire-and-forget `spawn_blocking` fails it immediately. ARP, DataLink and ICMP each had this bug
fixed separately and IS-IS was missed each time, which is why the guard covers all four in one
file.

### Kernel Interaction
- Kernel may handle some ICMP types automatically
- Echo Request may be answered by kernel before reaching userspace
- May need to use `SO_BINDTODEVICE` or disable kernel ICMP handling
- Testing should verify userspace server receives packets

### Interface Binding
The `interface` argument is **accepted but not honoured**: it is only used for log messages, and
the raw socket receives ICMP from every interface. The protocol's `default_binding()` is
`interface_based(DEFAULT_LOOPBACK_INTERFACE)` - `lo` on Linux/Windows, `lo0` on macOS/BSD - purely
so the flexible-binding plumbing has a value to carry.
- TODO: actually bind with `SO_BINDTODEVICE` (Linux) / `IP_BOUND_IF` (macOS).

### IPv6 Support
Not yet implemented:
- Current implementation only supports ICMPv4
- ICMPv6 uses different socket (Protocol::ICMPV6)
- ICMPv6 message types differ (e.g., Echo Request is type 128, not 8)
- Future enhancement

## Dual Logging

All logs use both tracing macros and status_tx:
```rust
console_info!(status_tx, "ICMP server listening...");
console_error!(status_tx, "Failed to parse: {}", err);
console_trace!(status_tx, "Packet hex: {}", hex::encode(data));
```

### Decision tags (LLM-failure path)

ICMP has no in-band failure reply. RFC 792 defines no "cannot answer" message for an echo
request, and RFC 1122 §3.2.2 forbids generating an ICMP error in response to an ICMP error, so a
synthesised Destination Unreachable would be a false statement about reachability rather than a
service-unavailable signal. When the LLM call fails, the server therefore **stays silent on the
wire on purpose** — but says so loudly in the log. Every packet ends with one `decision=` tag:

| Tag | Meaning |
|---|---|
| `model_reply` | a reply packet was actually put on the wire |
| `model_ignore` | the model answered with `ignore_icmp` only — deliberate silence |
| `fail_closed_no_action` | the model answered nothing at all |
| `fail_closed_action_error` | every action it produced failed to execute |
| `fail_closed_llm_error` | the LLM call itself errored; carries `category=overloaded\|unavailable` from `WireFailure::classify` |

Grep `decision=fail_closed_` to find every packet that went unanswered because no usable answer
was produced, as distinct from one the model chose to ignore. The error itself goes to the log
and the status stream only — nothing derived from it can reach a peer, because nothing is written
to the socket on these paths at all.

**Log Levels:**
- **ERROR**: Socket creation failures, send errors
- **INFO**: Server start, packet processing complete
- **DEBUG**: Packet summaries, LLM calls, action counts
- **TRACE**: Full packet hex dumps

## Security Considerations

### Legitimate Uses
- Network diagnostics (ping responses)
- ICMP honeypots for intrusion detection
- Network behavior research
- Traceroute simulation
- Time synchronization testing

### Potential Misuse
- ⚠️ ICMP flood attacks (DoS)
- ⚠️ ICMP redirect attacks
- ⚠️ ICMP tunneling for data exfiltration
- ⚠️ Network scanning without authorization

### Safeguards
- No default flooding behavior (LLM controls all sends)
- Rate limiting via LLM logic
- Requires explicit root access (cannot be run accidentally)
- Documentation warns about ethical use

## Example Prompts

### Basic Ping Responder
```
"Listen for ICMP echo requests on eth0 and reply to all pings with 'NetGet Pong' in the payload"
```

### Selective Responder
```
"Respond to pings from 192.168.1.0/24 but send destination unreachable for all other sources"
```

### Traceroute Simulation
```
"Act as a router that sends time exceeded messages for packets with low TTL"
```

### Honeypot Mode
```
"Log all ICMP traffic but don't send any replies - silent ICMP honeypot"
```


## Performance

### Latency
- Raw socket receive: < 1ms
- pnet packet parsing: < 0.1ms
- LLM call: 100-500ms (dominant factor)
- Packet construction: < 1ms
- Raw socket send: < 1ms

**Total RTT**: ~100-500ms (primarily LLM inference)

### Throughput
Limited by LLM processing:
- Sequential processing: ~2-10 packets/sec
- Parallel LLM calls: Higher throughput, depends on Ollama config
- Not suitable for high-rate ping responses

### Optimization Strategies
- **Scripting mode**: Pre-computed responses, bypass LLM
- **Static responses**: Hardcode echo replies for known patterns
- **Caching**: Remember LLM decisions for common cases

## Testing Notes

See `tests/server/icmp/CLAUDE.md` for test strategy and E2E test details.

## Future Enhancements

1. **Timestamp Request/Reply**: hand-build the 20-byte Type 13/14 messages, or wait for pnet
2. **IPv6 Support**: ICMPv6 with Neighbor Discovery
3. **Interface Binding**: Bind to specific interface parameter
4. **BPF Filtering**: Kernel-level packet filtering for efficiency
5. **Router Advertisement**: Full router simulation
6. **Multicast ICMP**: Group management messages

## References

- RFC 792 - Internet Control Message Protocol (ICMP)
- RFC 1191 - Path MTU Discovery
- RFC 1256 - ICMP Router Discovery
- pnet documentation: https://docs.rs/pnet/latest/pnet/
- socket2 documentation: https://docs.rs/socket2/latest/socket2/
