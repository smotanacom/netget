# Syslog Client E2E Tests

## Test Strategy

Black-box E2E tests using NetGet's UDP/TCP servers as syslog receivers.

## LLM Call Budget

**Target:** < 10 calls total
**Actual:** ~5 calls (UDP test, TCP test, custom messages test)

## Test Server Setup

No external server required - tests use NetGet's UDP/TCP servers as syslog receivers.

## Tests

1. **test_syslog_client_udp_connect** (2 LLM calls)
    - Start UDP server to receive syslog messages
    - Connect syslog client via UDP
    - Verify message sent successfully

2. **test_syslog_client_tcp_connect** (2 LLM calls)
    - Start TCP server to receive syslog messages
    - Connect syslog client via TCP with protocol parameter
    - Verify connection and message sent

3. **test_syslog_client_custom_messages** (1 LLM call)
    - Start UDP server
    - Send multiple syslog messages with different facilities/severities
    - Verify LLM can generate custom syslog messages

## Runtime

**Expected:** < 30 seconds total

## Known Issues

- None currently

## Syslog Message Format

Tests verify RFC 5424 format:

```
<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG
```

Example:

```
<13>1 2024-01-15T10:30:00.123Z netget netget - - - Test message from netget
```

Where:

- PRI = (Facility × 8) + Severity
- Facility: user (1)
- Severity: notice (5)
- PRI = (1 × 8) + 5 = 13

## `command_channel_test.rs`

Covers the dashboard's `[ send ]` path: `AppState::send_to_client` injects an action from
outside the client's own loop and the wire effect is asserted.

**LLM budget: 0 calls.** Every client event is routed to a `*` static handler that answers
with no actions (`try_execute_client_event_handler` runs before the budget debit), and the
client's LLM points at `http://127.0.0.1:1` as a second belt. No mock Ollama server is
needed or started.

`wait_for_client_handle` polls `AppState::has_client_handle` before the first send. That is
the regression guard for "register the channel **before** the connected-event LLM call" — if
registration moves after it, a parked connect event makes this poll time out.

Two tests, one per transport, because the byte count differs: UDP reports the datagram, TCP
reports the message **plus** its framing newline. The collectors are a plain `UdpSocket` and a
plain `TcpListener`; both compare the reported `Sent { bytes_sent }` against what actually
arrived and check the RFC 5424 PRI (`local0`/`info` = 134) and the message text.
