# IPSec/IKEv2 Honeypot Implementation

## Overview

A **receive-only IKE honeypot**. It parses IKEv1/IKEv2 datagrams, reports what it saw, and raises an event for the
LLM (or a script/static handler) to classify. It is **not a VPN**: it performs no cryptography, negotiates no Security
Associations, creates no tunnel interface, and **never transmits a byte back to the peer**.

For an actual working VPN tunnel in NetGet, use WireGuard (`src/server/wireguard/`) - that is the only VPN protocol
here with a real data plane.

**Status**: `DevelopmentState::Experimental`
**Privileges**: `PrivilegeRequirement::PrivilegedPort(500)` - no root needed beyond binding port 500
**Protocol Spec**: [RFC 7296 (IKEv2)](https://datatracker.ietf.org/doc/html/rfc7296)
**Ports**: UDP 500 (IKE), UDP 4500 (NAT-T)

## Implementation

### Custom parsing, no library

There is no external IKE dependency. All parsing is ~100 lines of manual byte handling in `mod.rs`:

- 28-byte IKE header (RFC 7296 §3.1): initiator/responder SPI, next payload, version, exchange type, flags,
  message ID, length
- Flag decomposition: Initiator (`0x08`), Version (`0x10`), Response (`0x20`)
- Payload-chain walk: follows `next_payload` links, bounds-checked against the datagram length, mapping type codes to
  names (SA, KE, IDi, IDr, CERT, CERTREQ, AUTH, NONCE, NOTIFY, DELETE, VENDOR, TSi, TSr, SK, CP, EAP)
- IKE version + exchange type classification (IKEv2 IKE_SA_INIT / IKE_AUTH / CREATE_CHILD_SA / INFORMATIONAL;
  IKEv1 Identity Protection / Aggressive Mode)

Parsing is defensive: undersized datagrams are dropped, the payload walk stops on any length that would run past the
end of the buffer or that is smaller than the 4-byte payload header, and no slice is indexed without a preceding
bounds check. A malformed or hostile datagram cannot panic the loop.

### Libraries considered and rejected

- **swanny** (GPL-3.0, git-only, experimental) - license is incompatible and the API is unstable; it was never added
  as a dependency. Earlier revisions of this document described swanny as if it were in use. It never was.
- **ipsec-parser** - parse-only; cannot build responses or establish SAs
- **strongSwan + VICI** - requires an external daemon, root, and XFRM kernel integration; violates NetGet's
  self-contained architecture
- **Custom full implementation** - SA negotiation, Diffie-Hellman, authentication, ESP, and kernel XFRM/SAD/SPD
  programming. A multi-month project, deliberately out of scope.

### Why it never replies

Silence is a design decision, not an omission:

- It cannot accidentally negotiate anything
- It does not fingerprint itself to scanners via distinctive responses
- It keeps the implementation small enough to be obviously correct

This includes the LLM-failure path. Where a request/response protocol must answer its peer with a category on
backend failure (`crate::utils::WireFailure`), IPSec has no response message to degrade: an error NOTIFY would be
both a fingerprint and an answer for a negotiation that never runs. So a failed `call_llm` is silent on the wire,
exactly like a successful classification.

Because the wire cannot distinguish the outcomes, the **log** must. `handle_handshake_initiation` tags every
outcome with a `decision=` field:

| `decision=` | Meaning |
|---|---|
| `model_reject` | the handler/model returned `reject_connection` |
| `model_answer` | the handler/model returned some other action |
| `no_answer` | the handler/model returned zero actions |
| `fail_closed_overloaded` | `call_llm` errored and the error classifies as `WireFailure::Overloaded` |
| `fail_closed_unavailable` | `call_llm` errored otherwise |

The error text goes to `tracing::error!` and the status stream only.

## LLM Integration

### Event flow

Every IKE handshake datagram (IKEv2 IKE_SA_INIT/IKE_AUTH, IKEv1 Identity Protection/Aggressive Mode) raises an
`ipsec_handshake` event through `crate::llm::action_helper::call_llm`. That helper tries a configured script or
static event handler first and only falls back to a real LLM call when none is configured, so a scripted IPSec
honeypot costs zero model calls.

Non-handshake exchanges (CREATE_CHILD_SA, INFORMATIONAL, unknown types) are logged but do not raise an event.

### Sync Actions (raised by `ipsec_handshake`)

All three are **classification/logging decisions**. None of them changes what goes on the wire, because nothing goes
on the wire.

1. **log_handshake** - record an analyst note. The primary action for this protocol.
2. **accept_connection** - label the attempt as legitimate. No SA is established.
3. **reject_connection** - label the attempt as unwanted, with a reason. The packet is dropped either way.

Removed from the advertised list (still accepted by `execute_action` for backwards compatibility):

- **send_notify** - the honeypot never transmits, so no IKE NOTIFY can be sent. The name promised wire behaviour the
  executor does not implement.
- **inspect_traffic** - there is no ESP traffic to inspect; no SA is ever established.

### Async Actions (User-triggered)

**None.** `get_async_actions()` returns an empty list.

`list_connections` and `close_connection` were previously advertised. IKE here is connectionless and no Security
Associations exist, so both were unconditional no-ops. Separately, the action executor calls `execute_action()` on a
stateless `IpsecProtocol` struct with no handle to the running server, so even a stateful version could not have
reached it.

### Event Types

- `ipsec_handshake` - an IKE handshake datagram was received and parsed

Data fields:

```json
{
  "peer_addr": "203.0.113.45:500",
  "packet_size": 256,
  "ike_version": "IKEv2",
  "exchange_type": "IKE_SA_INIT",
  "initiator_spi": "0123456789abcdef",
  "responder_spi": "0000000000000000",
  "is_initiator": true,
  "is_response": false,
  "message_id": 0,
  "payloads": ["SA", "KE", "NONCE"],
  "honeypot_mode": true,
  "responds_to_peer": false,
  "analysis": {
    "expected_payloads": "SA, KE, NONCE",
    "has_encryption": false,
    "has_vendor_id": false,
    "has_certificate": false
  }
}
```

An `ipsec_data` (ESP) event used to be declared. It was never emitted - no SA is ever established, so no ESP traffic
can be attributed to one - and has been removed rather than left as an event that a handler could subscribe to but
that would never fire.

## State Management

```rust
pub struct IpsecServer;  // no state
```

Stateless by design. Per NetGet's protocol rules there is no storage layer here: no SA table, no peer database, no
persistence. The LLM's memory and the log are the only record of what was seen.

## Task Lifecycle

The UDP receive loop's `JoinHandle` is registered via `AppState::register_server_task()`, so `stop_server` aborts it.

## Capabilities

Can:

- Detect IKE handshake attempts (IKEv1 and IKEv2)
- Extract SPIs, flags, message IDs, exchange types
- Enumerate the payload chain and flag encryption / vendor IDs / certificates
- Classify attempts via LLM, script, or static handler

Cannot:

- Complete IKE negotiation
- Establish Security Associations
- Encrypt or decrypt ESP traffic
- Authenticate clients
- Create tunnels or route traffic
- Send *any* response, including error NOTIFYs

## Future Work

A real IKEv2 responder is a large project, not a patch to this file. It would require:

1. SA proposal parsing and transform selection
2. Diffie-Hellman key exchange and SKEYSEED derivation
3. Authentication (PSK, and ideally certificate-based)
4. ESP encapsulation/decapsulation
5. Kernel XFRM SAD/SPD programming (Linux) or equivalent per-platform plumbing
6. IKE SA rekeying and fragmentation

Until then this stays an honest honeypot, and WireGuard remains NetGet's VPN.

## Examples

### Startup

```
netget> Start an IPSec/IKEv2 honeypot on port 500
```

```
[INFO] Starting IPSec/IKEv2 honeypot on 0.0.0.0:500 (parses and logs IKE, never replies)
[INFO] IPSec/IKEv2 honeypot listening on 0.0.0.0:500
```

### IKEv2 handshake detection

```
[TRACE] IPSec: IKEv2 IKE_SA_INIT from 203.0.113.45:500 (256 bytes, payloads=[SA, KE, NONCE])
[INFO] IPSec: IKEv2 handshake from 203.0.113.45:500 (payloads=[SA, KE, NONCE])
```

Handler or LLM response:

```json
{
  "actions": [
    {
      "type": "log_handshake",
      "details": "IKEv2 SA_INIT from 203.0.113.45 - single proposal, no vendor ID: likely a scanner"
    }
  ]
}
```

### Zero-LLM scripted mode

```json
{
  "type": "open_server",
  "port": 500,
  "base_stack": "ipsec",
  "event_handlers": [
    {
      "event_pattern": "ipsec_handshake",
      "handler": {
        "type": "static",
        "actions": [{"type": "log_handshake", "details": "IKE handshake detected"}]
      }
    }
  ]
}
```

## Use Cases

- Detecting IPSec scanning and reconnaissance
- Identifying misconfigured VPN clients pointed at the wrong host
- Fingerprinting IKE client implementations by payload chain and vendor IDs
- Studying IKE exchange sequences

**Not** for production VPN. Use WireGuard.

## References

- [RFC 7296 - IKEv2](https://datatracker.ietf.org/doc/html/rfc7296)
- [RFC 4301 - IPSec Architecture](https://datatracker.ietf.org/doc/html/rfc4301)
- [RFC 4303 - ESP](https://datatracker.ietf.org/doc/html/rfc4303)
