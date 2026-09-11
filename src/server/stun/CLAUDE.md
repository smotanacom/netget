# STUN Protocol Implementation

## Overview

STUN (Session Traversal Utilities for NAT) server implementing RFC 8489 (STUN - Session Traversal Utilities for NAT).
Provides NAT traversal assistance by informing clients of their public IP address and port as seen from the internet.

**Compliance**: RFC 8489 (STUN), RFC 5389 (obsolete, but widely deployed)

**Protocol Purpose**: STUN allows clients behind NAT to discover their external IP address and port mapping, essential
for WebRTC, VoIP, and peer-to-peer applications.

### Default behaviour: static, no LLM

A Binding response is **fully determined by the request** — reflect the source into
XOR-MAPPED-ADDRESS and echo the transaction ID — so **by default the server answers statically
with no LLM round-trip.** The model is consulted only when the operator **opts in**: a non-empty
server instruction, or a per-event handler configured for the binding event (gated by
`operator_wants_dynamic` in `mod.rs`, which is `has_instruction || has_handler`). Opt-in is how
you ask for non-standard behaviour such as lying about the mapped address. Even in opt-in mode,
if the LLM call fails the server **falls back to the correct static Binding response** rather
than erroring. See `send_static_binding_response` in `mod.rs`. The `## LLM Integration` section
below therefore describes the opt-in path, not the default one.

## Library Choices

**Manual Implementation** - Complete STUN protocol parsing implemented from scratch

- **Why**: STUN is a simple binary protocol (20-byte header + attributes)
- No complex Rust STUN server libraries available that integrate with LLM control
- Manual implementation provides full control over response generation

**Binary Protocol Handling**:

- Manual parsing of STUN message headers (20 bytes fixed)
- Attribute parsing for extensibility
- Network byte order (big-endian) for all multi-byte fields

## Architecture Decisions

### UDP-Based Protocol

**STUN uses UDP exclusively** (no TCP variant in RFC 8489):

- Connectionless: Each binding request is independent
- Stateless server: No session tracking required
- Fast response: Single request-response round trip

**Connection Tracking**:

- Each STUN request creates a "connection" in NetGet UI
- Connection ID represents a single transaction
- Closes immediately after response sent

### Message Format

**STUN Message Structure** (RFC 8489 Section 6):

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|0 0|     STUN Message Type     |         Message Length        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         Magic Cookie (0x2112A442)             |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                     Transaction ID (96 bits)                  |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                     Attributes (variable)                     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

**Message Type Encoding**:

- Method: Binding (0x0001)
- Class: Request (0x00), Success Response (0x01), Error Response (0x10)
- Encoding: 0bMMMMMMMMMMCCCCMM (M=method bits, C=class bits)

**Example**:

- Binding Request: 0x0001
- Binding Success Response: 0x0101
- Binding Error Response: 0x0111

### Request Processing Flow

1. **Receive UDP packet** → Parse STUN header
2. **Validate magic cookie** (0x2112A442) → Reject if invalid
3. **Extract transaction ID** (12 bytes) → Must echo in response
4. **Parse message type** → Determine method and class
5. **Create event** → `STUN_BINDING_REQUEST_EVENT` with peer_addr, transaction_id
6. **Static default (no operator policy)** → build the Binding Success Response directly, **no LLM call**.
   **Opt-in only** (instruction or handler configured) → consult the handler/LLM to generate the response
7. **Build response** → Copy transaction ID, add XOR-MAPPED-ADDRESS attribute
8. **Send UDP response** → To client's IP:port

### Attribute Handling

**Common STUN Attributes**:

- **XOR-MAPPED-ADDRESS** (0x0020): Client's public IP:port (XOR-encoded with magic cookie)
- **MAPPED-ADDRESS** (0x0001): Client's public IP:port (plain, legacy)
- **SOFTWARE** (0x8022): Server software identification (optional)
- **FINGERPRINT** (0x8028): CRC32 checksum (optional)

**XOR Encoding** (RFC 8489 Section 14.1):

- Port: XOR with upper 16 bits of magic cookie (0x2112)
- IPv4: XOR each byte with magic cookie (0x2112A442)
- IPv6: XOR with magic cookie + transaction ID

**Why XOR**: Prevents NATs from rewriting addresses embedded in STUN payloads.

## LLM Integration

### Action-Based Response Generation

**LLM generates complete STUN response** via action:

```json
{
  "actions": [
    {
      "type": "send_stun_binding_response",
      "transaction_id": "0102030405060708090a0b0c",
      "mapped_address": "203.0.113.5:54321",
      "xor_mapped_address": true,
      "software": "NetGet STUN/1.0"
    }
  ]
}
```

**Action Parameters** (these are the names the executor actually reads; the docs
previously listed `client_address`, `client_port` and `xor_mapped`, none of which
exist, so a response built from them failed with "Missing 'mapped_address' field"
and the client timed out):

- `transaction_id`: Hex string, exactly 24 hex chars / 12 bytes. **Must** match the
  request; clients discard responses whose transaction ID differs.
- `mapped_address`: Single `"ip:port"` string to return (usually the event's
  `peer_addr`)
- `xor_mapped_address`: true = XOR-MAPPED-ADDRESS (default), false = MAPPED-ADDRESS
- `software`: Optional SOFTWARE attribute value (default `"NetGet/1.0"`)

There is deliberately **no `message_integrity` parameter**. One used to be declared,
described to the model as "Include MESSAGE-INTEGRITY attribute", and thrown away by the
executor — the flag did nothing whichever way it was set. RFC 8489 §14.6 computes that
attribute over a key derived from a username/realm/password this server neither holds nor
has any way to obtain, so it could never have been honoured; the parameter is gone rather
than documented-as-inert, because an inert knob is one the model will keep reaching for.

### Echoing the transaction ID without an LLM call

Static handlers interpolate event fields, so a zero-LLM STUN server is expressible
directly:

```json
{
  "type": "static",
  "actions": [{
    "type": "send_stun_binding_response",
    "transaction_id": "{{event.transaction_id}}",
    "mapped_address": "{{event.peer_addr}}",
    "xor_mapped_address": true
  }]
}
```

Verified with a raw UDP STUN client: the response echoes the request's transaction
ID and its XOR-MAPPED-ADDRESS decodes to the client's real source address.

**Action Execution**:

1. Parse action parameters
2. Decode transaction ID from hex
3. Build STUN Binding Success Response (0x0101)
4. Add XOR-MAPPED-ADDRESS attribute with XOR-encoded IP:port
5. Optionally add SOFTWARE attribute
6. Send as UDP datagram to client

### Event Type

**`STUN_BINDING_REQUEST_EVENT`**:

- Triggered: Valid STUN Binding Request received
- Context:
    - `peer_addr`: Client's IP:port as seen by server (their public address)
    - `local_addr`: Server's listening IP:port
    - `transaction_id`: Hex-encoded transaction ID (for response matching)
    - `message_type`: "BindingRequest"
    - `bytes_received`: Request size
- This event is only raised to the handler/LLM when the operator has opted in; the default path answers it statically before any handler is consulted

**No filtering logic**: All valid STUN requests receive responses (STUN is inherently public).

## Connection and State Management

**Per-Request State** (`ProtocolConnectionInfo::Stun`):

```rust
Stun {
    transaction_id: Option<String>, // Hex-encoded transaction ID
}
```

**Stateless Server**: No connection tracking beyond single request. Each request:

1. Creates connection entry in UI
2. Processes request
3. Sends response
4. Closes immediately

**No Session Management**: STUN has no concept of sessions or persistent connections.

## Protocol Validation

### Request Validation Checks

`parse_stun_header` in `mod.rs` returns `(transaction_id, message_type, servable)`, and
`servable` means "this server may answer it", not "this parsed". A datagram must clear all
of these:

1. **Minimum Length**: At least 20 bytes (header only)
2. **Leading bits**: The top two bits of the message type are zero (RFC 8489 §5). `0b01`
   there is a TURN ChannelData frame, not STUN.
3. **Magic Cookie**: Bytes 4-7 must be 0x2112A442
4. **Declared Length**: A multiple of 4, and `20 + length` must not exceed the datagram
5. **Message Type**: Must be Binding **Request** (class=0, method=1)
6. **Transaction ID**: Extract 12 bytes (bytes 8-19)

**Anything else: silently ignored** — no error response, per RFC 8489.

Check 5 is a security control, not pedantry, and it was missing until September 2026: any
datagram carrying the magic cookie was answered, including a Binding **Response**. See
`## Security Considerations` below and `tests/server/stun/reflection_test.rs`.

**Message class decoding**: class is `C1<<1 | C0` where C0 is bit 4 and C1 is bit 8
(RFC 8489 section 5), giving 0 = request, 1 = indication, 2 = success response,
3 = error response. An earlier expression collapsed the two class bits incorrectly
and decoded every response class as 18 or 19, so responses were labelled "Unknown".
This only ever affected labelling, since a server receives requests.

### Response Generation

**Minimal Valid Response**:

```
[Message Type: 0x0101 (Success)]
[Message Length: 12 (XOR-MAPPED-ADDRESS attribute)]
[Magic Cookie: 0x2112A442]
[Transaction ID: <copy from request>]
[Attribute Type: 0x0020 (XOR-MAPPED-ADDRESS)]
[Attribute Length: 8 (family + port + IPv4)]
[Family: 0x01 (IPv4)]
[X-Port: port XOR 0x2112]
[X-Address: ip XOR 0x2112A442]
```

## Limitations

### Current Limitations

1. **IPv6 is encoded, but untested against a real IPv6 client**
    - `add_xor_mapped_address_attribute` and `add_mapped_address_attribute` both handle
      `SocketAddr::V6` (family 0x02, first 4 bytes XORed with the magic cookie and the
      remaining 12 with the transaction ID, per RFC 8489 §14.2). This section used to say
      IPv6 "not implemented"; that was wrong, and under-claiming is as misleading as
      over-claiming.
    - No test binds an IPv6 socket, so the encoding is asserted by nothing.

2. **No Authentication**
    - RFC 8489 defines MESSAGE-INTEGRITY and USERNAME attributes
    - Not implemented (STUN is typically used without auth in public servers)

3. **No TURN Support**
    - STUN only provides IP discovery
    - Does not relay traffic (use separate TURN server for relay)

4. **Minimal Attribute Support**
    - Only XOR-MAPPED-ADDRESS/MAPPED-ADDRESS, SOFTWARE and ERROR-CODE are **written**
    - Inbound attributes are not parsed at all: the server reads the 20-byte header and
      nothing past it, so there is no TLV walk here to drive out of bounds
    - No FINGERPRINT, MESSAGE-INTEGRITY, REALM, NONCE, and no action can request one

5. **No UDP Retransmission Handling**
    - STUN clients typically retry on timeout
    - Server doesn't track or deduplicate retries

### Protocol Compliance Gaps

**RFC 8489 Features Not Implemented**:

- Alternate Server (ALTERNATE-SERVER attribute)
- Error responses (400 Bad Request, 500 Server Error)
- Authentication (MESSAGE-INTEGRITY, USERNAME, REALM, NONCE)
- Fingerprint (FINGERPRINT attribute with CRC32)
- IPv6 support
- Backwards compatibility with RFC 3489/5389

**Impact**: Sufficient for basic WebRTC/STUN usage. Not suitable for enterprise deployments requiring authentication.

## Use Cases

### WebRTC NAT Traversal

**Typical Flow**:

1. WebRTC client sends STUN Binding Request to stun.example.com:3478
2. STUN server responds with client's public IP:port
3. Client includes this in ICE candidate exchange
4. Peer-to-peer connection established using discovered address

### VoIP Configuration

**SIP/SDP Integration**:

1. VoIP client behind NAT doesn't know public IP
2. Queries STUN server to discover public IP:port
3. Includes discovered address in SIP INVITE or SDP offer
4. Remote peer can reach client through NAT

### Network Diagnostics

**NAT Type Detection** (using STUN with additional queries):

- Full Cone NAT: Same mapping for all destinations
- Restricted Cone NAT: Port reuse, filtered by source IP
- Port Restricted Cone NAT: Port reuse, filtered by source IP:port
- Symmetric NAT: Different mapping per destination (STUN insufficient)

## Performance Considerations

**Stateless Operation**: Each request O(1) processing, no memory growth.

**UDP Overhead**: Minimal (20-byte header + ~12-byte attribute = 32 bytes response).

**LLM Latency**: 500ms-5s per request. Acceptable for STUN (not latency-sensitive like media).

**Concurrent Requests**: Handled in parallel via tokio. Thousands of requests/second possible (limited by LLM
throughput).

## Example Prompts

### Basic STUN Server

```
Start a STUN server on port 3478. For all binding requests, return the client's
public IP address and port.
```

### STUN with Custom Software Identifier

```
Start a STUN server on port 3478. Include SOFTWARE attribute "NetGet-STUN/1.0"
in all responses.
```

### Logging STUN Requests

```
Start a STUN server on port 3478. Log the source IP and port of every binding
request before responding.
```

### STUN with Response Delay (for testing)

```
Start a STUN server on port 3478. Wait 2 seconds before sending each response
(simulating high latency).
```

## Security Considerations

**Only a Binding Request is answered.** This is the whole reflection surface, and it was
wrong until September 2026: `parse_stun_header` returned valid for *any* datagram carrying
the magic cookie, so a Binding **Response** was answered with another Binding response. Two
consequences, each reachable with one forged packet:

- **A response loop.** Spoof one datagram whose source address is a second NetGet STUN
  server and the two answer each other's answers indefinitely. Nothing in either side
  breaks the cycle — not a transaction-ID cache, not a rate limit, neither exists.
- **Reflected amplification.** The reply is bigger than the request, so a spoofed source
  turns this server into an amplifier aimed at a third party.

Both are closed by refusing every class but Request. `tests/server/stun/reflection_test.rs`
pins it, with a genuine Binding Request as the control — otherwise "no reply" would be
indistinguishable from an unreachable server.

**Amplification Attack Potential**: a 20-byte request draws a 48-byte reply
(20-byte header + 12-byte XOR-MAPPED-ADDRESS + 16-byte SOFTWARE), so roughly **2.4x**.
That is inherent to STUN and no worse than any real STUN server, but it is not the
"similar size, not suitable for amplification" this section used to claim. Shrinking it
further would mean dropping SOFTWARE, which is a legitimate thing to want from a honeypot.

**IP Spoofing**: UDP allows spoofed source IPs. The server must NOT trust the client IP for
authentication — and cannot, since none is implemented.

**Rate Limiting**: **there is none.** No per-source limit, no transaction-ID cache, no
duplicate suppression: every well-formed Binding Request is answered, as fast as they
arrive. On an untrusted network put a rate limiter in front of it.

## References

- RFC 8489: Session Traversal Utilities for NAT (STUN)
- RFC 5389: Session Traversal Utilities for NAT (obsolete, but widely deployed)
- RFC 5780: NAT Behavior Discovery Using STUN
- WebRTC STUN Usage: https://developer.mozilla.org/en-US/docs/Web/API/RTCIceServer
- STUN Message Structure: https://datatracker.ietf.org/doc/html/rfc8489#section-6

## Failure behaviour: the correct static response, not an error

When `call_llm` returns `Err`, the request is answered with the ordinary **Binding Success
Response** — the client's real reflected address, its transaction ID echoed — by
`send_static_binding_response` in `mod.rs`, and the backend error goes to the log and the
status stream only. Covered by `tests/server/stun/llm_failure_test.rs`.

This is a fail-*closed* answer despite looking permissive, and STUN is the rare protocol
where that is true: a Binding response is neither a credential nor an approval, it is a fact
about the requester's own source address, and it is exactly what the server would have sent
had the operator never opted into LLM control. The model is consulted here only to permit
*lying* about that address; falling back to the truth withholds the model's influence rather
than granting anything. Compare `src/server/turn/`, where the same failure must grant
nothing, and `src/server/radius/`, where it must refuse — there the reply *is* an assertion.

**This section used to describe a 500 Binding Error Response instead**, matching an earlier
implementation. `StunProtocol::build_error_response` survives from it and is now reached only
through the `send_stun_error_response` action, i.e. when a handler or the model refuses a
request deliberately.
