# NTP Protocol Implementation

## Overview

NTP (Network Time Protocol) server implementing RFC 5905 for time synchronization.

### Default behaviour: static, no LLM

A normal time response is **mechanical** — stratum 2, LOCL reference id, current-time timestamps,
with the origin timestamp and version echoed from the request — so **by default the server answers
statically with no LLM round-trip.** The model is consulted only when the operator **opts in**: a
non-empty server instruction, or a per-event handler configured for the request event (gated by
`should_call_llm` in `mod.rs` = `has_instruction || has_handler`). Opt-in is how you make the
server skew or lie about the time. Even in opt-in mode, if the LLM call fails the server **falls
back to the correct static time response** (`send_static_time_response` in `mod.rs`). The
`Action-Based LLM Control` and `LLM Integration` material below describes that opt-in path; it is
not invoked on the default path.

**Status**: Beta (Core Protocol)
**RFC**: RFC 5905 (NTPv4), RFC 1305 (NTPv3)
**Port**: 123 (UDP)
**Privilege**: declares `PrivilegeRequirement::PrivilegedPort(123)`; any port above 1023 needs none.
**Test coverage**: `tests/server/ntp/test.rs` (note the file is `test.rs`, not `e2e_test.rs`). It
drives the server with `rsntp` and falls back to a raw 48-byte packet — but the fallback only prints
its outcome and never asserts, so the suite passes even if the client rejects every reply. Treat it
as a smoke test, not proof of client compatibility.

## Library Choices

- **Manual NTP packet construction** - No external library
    - NTP protocol is simple enough to implement directly
    - 48-byte fixed-size packet structure
    - Minimal parsing required (just extract client's transmit timestamp)
    - Custom `build_ntp_packet()` function constructs responses

**Rationale**: Unlike DNS/DHCP, NTP packet structure is very simple (fixed 48 bytes, mostly timestamps). Using a library
would add unnecessary dependency. Manual implementation provides full control and is easier to understand.

**Note**: ntpd-rs exists but is a full NTP daemon implementation, not a simple protocol library.

## Architecture Decisions

### 1. Action-Based LLM Control

The LLM responds with a single action:

- `send_ntp_time_response` - Send time synchronization response
- `send_ntp_response` - Send raw hex packet (advanced)
- `ignore_request` - No response

The `send_ntp_time_response` action has many optional parameters (stratum, precision, timestamps), all with sensible
defaults. LLM typically only needs to specify stratum level.

### 2. Automatic Origin Timestamp Echo

NTP requires the server to echo the client's transmit timestamp as the origin timestamp:

1. Client sends a request with transmit_timestamp = T1
2. The reply must carry T1 verbatim in bytes 24-31
3. The client matches the reply to its request by that value and uses it for round-trip delay —
   `rsntp`, `chrony` and `ntpdate` all discard a reply whose origin timestamp is anything else,
   which surfaces as a timeout rather than an error

**Implementation**: the socket loop reads bytes 40-47 of the request and builds a per-request
`NtpProtocol::for_request(origin, version)` handler. `send_ntp_time_response` writes that value into
the reply unless the action explicitly overrides `origin_timestamp`. Because the handler instance
belongs to one datagram, concurrent requests cannot pick up each other's timestamps.

Two things this deliberately does *not* do:

- It does not fall back to the current time when the request carried no usable timestamp (a request
  shorter than 48 bytes). Those bytes stay zero — a visible mismatch beats a fabricated one.
- It does not modify the action list after `call_llm` returns. An earlier version tried to inject
  `origin_timestamp` into `execution_result.raw_actions` after the call, but `call_llm` has already
  executed the actions and built the packet by then, so that code never affected a single byte on
  the wire and the origin timestamp was always zero-or-now. Any similar post-hoc mutation of
  `raw_actions` is dead code.

### 2b. Version Echo

RFC 5905 says a server answers in the version the client used. Byte 0's version field is copied from
the request (clamped to 1-4, default 4), so an NTPv3 client gets a v3 reply. Mode is always 4
(server).

### 3. Flexible Timestamp Format

Timestamps can be provided as:

- `"current_time"` - Use system's current time (most common)
- Unix timestamp (seconds since 1970) - Converted to NTP format
- Raw NTP timestamp (64-bit: seconds + fraction) - Used as-is
- `null` or omitted - Defaults to current_time

**NTP Epoch**: NTP uses January 1, 1900 as epoch (2,208,988,800 seconds before Unix epoch)

### 4. Sensible Defaults

LLM doesn't need to understand NTP packet structure. Default values:

- `leap_indicator`: 0 (no warning)
- `stratum`: 2 (secondary time source)
- `poll`: 6 (64-second polling interval)
- `precision`: -20 (~1 microsecond)
- `root_delay`: 0.0 (no delay)
- `root_dispersion`: 0.0 (no error)
- `reference_id`: "" (empty)
- All timestamps: current_time

LLM can override any field if instructed (e.g., "act as stratum 1 server").

### 5. Dual Logging

- **DEBUG**: Request summary ("NTP received 48 bytes from 127.0.0.1")
- **TRACE**: Full hex dump of NTP packets (both request and response)
- Both go to netget.log and TUI Status panel

### 6. Connection Tracking

Each NTP request creates a "connection" entry:

- Connection ID: unique per request
- Protocol info: `ProtocolConnectionInfo::empty()` — no NTP-specific payload is attached
- Records the peer address, byte and packet counts
- Status: Active. Entries are never reaped, so a busy server accumulates one per request

## LLM Integration

### Event Type

**`ntp_request`** - Triggered when NTP client sends time request

Event parameters:

- `current_time` (number) - server's current time as a Unix timestamp
- `client_transmit_timestamp` (number, optional) - the client's transmit time as the **raw 64-bit
  NTP value**; absent when the request was shorter than 48 bytes
- `client_transmit_unix` (number, optional) - the same value in whole Unix seconds, lossy, for
  readability only
- `client_version` (number) - NTP version of the request, echoed in the reply
- `client_mode` (number) - mode field of the request (3 = normal client query)
- `bytes_received` (number) - size of the received datagram; > 48 means extension fields or an
  authentication MAC, which are ignored

### Available Actions

#### `send_ntp_time_response`

Send NTP time synchronization response. Most fields have sensible defaults.

Parameters (all optional except where noted):

- `leap_indicator` - Leap second warning: 0=none, 1=+1s, 2=-1s, 3=unsync (default: 0)
- `stratum` - Stratum level: 0=unspec, 1=primary, 2-15=secondary (default: 2)
- `poll` - Poll interval as log2(seconds): 4=16s, 6=64s, 10=1024s (default: 6)
- `precision` - Clock precision as log2(seconds), negative values (default: -20)
- `root_delay` - Round-trip delay to primary reference in seconds (default: 0.0)
- `root_dispersion` - Max error relative to primary reference in seconds (default: 0.0)
- `reference_id` - 4-char identifier: "LOCL", "GPS.", "PPS.", "ATOM", or IP (default: "")
- `reference_timestamp` - When clock was last set (default: current_time)
- `origin_timestamp` - leave unset; the server copies the client's transmit time from the request
- `receive_timestamp` - When server received request (default: current_time)
- `transmit_timestamp` - When server sends response (default: current_time)

**Note**: leave `origin_timestamp` unset — the server writes the correct value.

#### `send_ntp_response` (Advanced)

Send a complete packet as hex, at least 96 hex characters (48 bytes). The field is decoded strictly
as hex: whitespace, `:` separators and a leading `0x` are stripped, anything else that fails to
decode returns an error instead of being sent as literal ASCII. Nothing is echoed into a raw packet,
so bytes 24-31 must hold the client's transmit timestamp or the reply will be discarded.

#### `ignore_request`

Don't send any response.

### Example LLM Response

```json
{
  "actions": [
    {
      "type": "send_ntp_time_response",
      "stratum": 2
    },
    {
      "type": "show_message",
      "message": "Sent NTP time response as stratum 2 server"
    }
  ]
}
```

### Example: Stratum 1 Server

```json
{
  "actions": [
    {
      "type": "send_ntp_time_response",
      "stratum": 1,
      "reference_id": "GPS.",
      "precision": -20,
      "root_delay": 0.001,
      "root_dispersion": 0.001
    }
  ]
}
```

## Connection Management

### Connection Lifecycle

1. **Request Received**: UDP datagram on port 123
2. **Parse**: Extract client's transmit timestamp (bytes 40-47)
3. **Register**: Create ConnectionId and add to ServerInstance
4. **Process**:
   - **Default (no operator policy)** → build the static time response directly, **no LLM call**
   - **Opt-in only** (instruction or handler) → call the handler/LLM with the `ntp_request` event
5. **Auto-Inject**: Echo origin_timestamp if not provided
6. **Build**: Construct 48-byte NTP response packet
7. **Respond**: Send UDP response
8. **Update**: Track bytes/packets sent/received
9. **Persist**: Connection remains in UI

### NTP Packet Structure

Fixed 48-byte packet:

- Byte 0: LI (2 bits) + Version (3 bits) + Mode (3 bits)
- Byte 1: Stratum
- Byte 2: Poll interval
- Byte 3: Precision
- Bytes 4-7: Root delay (32-bit fixed point)
- Bytes 8-11: Root dispersion (32-bit fixed point)
- Bytes 12-15: Reference ID (4 ASCII chars)
- Bytes 16-23: Reference timestamp (64-bit NTP timestamp)
- Bytes 24-31: Origin timestamp (64-bit NTP timestamp)
- Bytes 32-39: Receive timestamp (64-bit NTP timestamp)
- Bytes 40-47: Transmit timestamp (64-bit NTP timestamp)

**NTP Timestamp Format**: 64 bits = 32-bit seconds + 32-bit fraction (1/2^32 seconds precision)

## Known Limitations

### 1. NTPv1-v4 Only

- Accepts any version in requests and answers in the same version (default 4)
- No NTPv5 support (still in draft)
- The mode field of the request is reported to the LLM as `client_mode` but not enforced: a mode 4
  or mode 5 packet is answered as if it were a client query, so pointing two NetGet NTP servers at
  each other would loop

### 2. No NTP Extensions

- No extension fields beyond 48-byte basic packet
- No authentication (NTP extensions)
- No autokey or symmetric key authentication

### 3. No Stratum 0

- Server can't act as primary reference clock (stratum 0 is reserved)
- Can act as stratum 1 (primary) or 2-15 (secondary)
- No integration with actual hardware clocks (GPS, atomic, etc.)

### 4. Simplified Time Handling

- Uses system time (SystemTime::now())
- No clock discipline algorithms
- No tracking of time offset or drift
- Just returns current system time with NTP formatting

### 5. No NTP Control Protocol

- Only implements client/server mode (mode 3 request → mode 4 response)
- No broadcast mode (mode 5)
- No NTP control messages (ntpq/ntpdc protocol)

### 6. No Kiss-of-Death

- Doesn't send Kiss-of-Death (KoD) packets to rate-limit clients
- No rate limiting or denial-of-service protection

## Example Prompts

### Basic NTP Server

```
listen on port 123 via ntp
Respond to NTP requests with the current system time
Use stratum 2
```

### Stratum 1 Server

```
listen on port 123 via ntp
Act as a stratum 1 NTP server
Use reference ID "GPS." to indicate GPS time source
Set precision to -20 (1 microsecond)
```

### Custom Stratum Server

```
listen on port 123 via ntp
Act as a stratum 3 NTP server
Reference identifier: "LOCL" (local clock)
Poll interval: 6 (64 seconds)
```

### High-Precision Server

```
listen on port 123 via ntp
Respond with:
  - Stratum 1
  - Reference ID: "ATOM" (atomic clock)
  - Precision: -30 (1 nanosecond)
  - Root delay: 0.0001 seconds
  - Root dispersion: 0.00001 seconds
```

## Performance Characteristics

### Latency

- **With Scripting**: Sub-millisecond (script handles requests)
- **Without Scripting**: 2-5 seconds (one LLM call per request)
- Packet construction: ~5-10 microseconds (very fast)
- Timestamp extraction: ~1-2 microseconds

### Throughput

- **With Scripting**: Tens of thousands of requests per second
- **Without Scripting**: Limited by LLM (~0.2-0.5 requests/sec)
- NTP traffic is very low volume (clients poll every 64-1024 seconds)

### Scripting Compatibility

NTP is excellent candidate for scripting:

- Extremely simple logic (just return current time)
- No state machine
- Deterministic responses
- Very high query rate potential

When scripting enabled:

- Server startup generates script (1 LLM call)
- All requests handled by script (0 LLM calls)
- Script can get system time and format NTP response instantly

### Time Accuracy

- Accuracy limited by system clock (typically ±1-100ms)
- No hardware clock integration
- No clock discipline or offset correction
- Good enough for testing, not for production time service

## References

- [RFC 5905: Network Time Protocol Version 4](https://datatracker.ietf.org/doc/html/rfc5905)
- [RFC 1305: Network Time Protocol Version 3](https://datatracker.ietf.org/doc/html/rfc1305)
- [NTP Packet Format](https://www.rfc-editor.org/rfc/rfc5905.html#section-7.3)
- [NTP Timestamp Format](https://www.rfc-editor.org/rfc/rfc5905.html#section-6)
- [NTP Stratum Levels](https://www.ntp.org/reflib/book/ch11/)
- [ntpd-rs (Rust NTP daemon)](https://github.com/pendulum-project/ntpd-rs)

## Failure behaviour

**This section previously described a Kiss-o'-Death failure path. The code does not do that,
and has not for some time.** `actions::build_kod_packet` exists, compiles and is correct —
and is called by nothing: `grep -rn build_kod_packet src/` finds the definition and this
file's old claim, no call site. What `mod.rs` actually does on `call_llm` returning `Err` is
send the **mechanical static time response** — LI 0, stratum 2, LOCL, true current time — and
`tests/server/ntp/llm_failure_test.rs` asserts exactly that, down to
`assert_eq!(buf[1], 2, "… not a Kiss-o'-Death (stratum 0)")`. The doc and the test have
contradicted each other in this directory; the test is the one that runs.

### The honest name for the LLM-failure path is not "fail closed"

A stratum-2 reply is a **positive assertion**: the client takes it as a usable time sample and
sets its clock. So on backend failure the peer gets an affirmative answer, which is why that
path is tagged `decision=static_default_llm_error` and **not** `decision=fail_closed_*` —
`grep decision=fail_closed` will not find an NTP backend outage, deliberately, because the
server did not deny anything.

The argument for the behaviour is that the mechanical answer is the server's *own clock*, so
it cannot be a lie in the operator's favour: an operator who opted into the model in order to
skew the time simply gets the truth instead. That is a real argument and the behaviour is
left as it is. It is not the same as failing closed, and the log must not pretend otherwise.
If it is ever revisited, `build_kod_packet` is the alternative already sitting in
`actions.rs`, and a KoD is the one reply NTP defines that is not a time sample.

### Every terminal outcome

| Outcome | On the wire | Log |
|---|---|---|
| No operator policy at all (`operator_wants_dynamic` false) — no model call is made | mechanical stratum-2 time response | INFO `decision=static_default` |
| Model answered with `send_ntp_time_response` / `send_ntp_response` | that packet | INFO `NTP request from <peer> decision=model_answer` |
| Model answered `ignore_request` | **nothing** | INFO `decision=model_reject` |
| Model answered with no usable action | **nothing**; the client keeps polling | WARN `decision=model_silent` |
| Executor refused the model's action (bad hex, short packet, unknown action) | **nothing** | ERROR `decision=fail_closed_bad_action` naming the action and the reason |
| Backend failed or was saturated | mechanical stratum-2 time response — **an affirmative answer**, see above | ERROR `decision=static_default_llm_error category=overloaded\|unavailable` with the full error |
| The server's own static action failed to encode | nothing | ERROR/WARN `decision=fail_closed_action_error` |

Both static sends carry the token on their own line too
(`NTP static time response to <peer> (48 bytes) decision=…`), so the two are never conflated.

The error text never reaches the wire: an NTP reply is 48 fixed binary bytes with no free-text
field, so there is nothing to leak into. `WireFailure::classify` is used only to put
`category=overloaded` / `category=unavailable` in the log.

Covered by `tests/server/ntp/llm_failure_test.rs` (decodes all 48 bytes by hand) and
`tests/server/ntp/decision_tag_test.rs` (asserts the tag on the failure path).
