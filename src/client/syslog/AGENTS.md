# Syslog Client Implementation

## Overview

The syslog client implementation provides LLM-controlled outbound connections to syslog servers. The LLM can send log
messages with configurable facility, severity, and message content. Both UDP and TCP transports are supported.

## Implementation Details

### Library Choice

- **Custom RFC 5424 implementation** - Manual message formatting
- **tokio::net::TcpStream** - TCP transport
- **tokio::net::UdpSocket** - UDP transport (default)
- **chrono** - Timestamp generation

### Architecture

```
┌────────────────────────────────────────┐
│  SyslogClient::connect_with_llm_actions│
│  - Parse protocol (TCP/UDP)            │
│  - Connect to syslog server            │
│  - Call LLM with connected event       │
│  - Execute send_syslog_message actions │
└────────────────────────────────────────┘
```

### Syslog Protocol

**RFC 5424 Format:**

```
<PRI>VERSION TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG
```

**Components:**

- **PRI**: Priority = (Facility × 8) + Severity
- **VERSION**: 1 (RFC 5424)
- **TIMESTAMP**: RFC 3339 timestamp
- **HOSTNAME**: Hostname or "-"
- **APP-NAME**: Application name or "-"
- **PROCID**: Process ID or "-"
- **MSGID**: Message ID or "-"
- **STRUCTURED-DATA**: "-" (not implemented)
- **MSG**: The actual log message

**Facilities (0-23):**

- 0: kern (kernel)
- 1: user
- 2: mail
- 3: daemon
- 4: auth
- 5: syslog
- 6: lpr
- 7: news
- 8: uucp
- 9: cron
- 10: authpriv
- 11: ftp
- 12: ntp
- 13: security
- 14: console
- 15: solaris-cron
- 16-23: local0-local7

**Severities (0-7):**

- 0: emerg (Emergency)
- 1: alert
- 2: crit (Critical)
- 3: err (Error)
- 4: warning
- 5: notice
- 6: info (Informational)
- 7: debug

### Transport Protocols

**UDP (Default):**

- Connectionless, fire-and-forget
- Port 514 (standard syslog)
- Message as single datagram

**TCP:**

- Connection-oriented
- Port 514 or custom
- Message terminated with newline

### LLM Control

**Async Actions** (user-triggered):

- `send_syslog_message` - Send log message with facility, severity, message
    - Parameters: facility, severity, message, hostname, app_name, proc_id, msg_id
- `disconnect` - Close connection (TCP only)

**Events:**

- `syslog_connected` - Fired when connection established
- `syslog_message_sent` - Fired when message sent (future)

### Data Encoding

**LLM-Friendly:**

- Facility and severity as strings (e.g., "user", "info")
- Message as plain text
- Automatic RFC 5424 formatting

### Dual Logging

```rust
info!("Syslog client {} sent message: facility={}, severity={}, msg={}",
    client_id, facility, severity, message);  // → netget.log
status_tx.send(format!("[CLIENT] Syslog {} sent: [{}:{}] {}",
    client_id, facility, severity, message)); // → TUI
```

### Connection Lifecycle

**UDP:**

1. **Bind**: Bind to ephemeral local port
2. **Send**: Send datagram to remote server
3. **No Response**: Fire-and-forget

**TCP:**

1. **Connect**: `TcpStream::connect(remote_addr)`
2. **Connected**: Update ClientStatus::Connected
3. **Send**: Write message + newline
4. **Disconnect**: Close connection

### Error Handling

- **Connection Failed** (TCP): Return error, client stays in Error state
- **Bind Failed** (UDP): Return error
- **Send Error**: Log, continue
- **Invalid Facility/Severity**: Return error
- **LLM Error**: Log, continue

## Limitations

- **No TLS Support** - Plain TCP/UDP only (TLS-syslog could be added)
- **No Structured Data** - SD-ELEMENT not implemented
- **No Message Acknowledgement** - Fire-and-forget (even TCP)
- **No Server Response Handling** - One-way logging only
- **No Syslog Server Discovery** - Must specify address explicitly

## Testing Strategy

See `tests/client/syslog/CLAUDE.md` for E2E testing approach.

## Example Usage

**UDP Syslog:**

```
open_client syslog localhost:514 "Send user info logs"
```

**TCP Syslog:**

```
open_client syslog localhost:514 --protocol tcp "Send daemon error logs"
```

**LLM Action:**

```json
{
  "type": "send_syslog_message",
  "facility": "user",
  "severity": "info",
  "message": "Test message from netget",
  "hostname": "netget-host",
  "app_name": "netget"
}
```

## References

- **RFC 5424**: The Syslog Protocol (https://tools.ietf.org/html/rfc5424)
- **RFC 3164**: The BSD syslog Protocol (legacy, not implemented)

## Command channel (dashboard `[ send ]`)

`AppState::send_to_client` can inject an action into the running client — for syslog this is
close to the whole point of the client, since "emit this line now" is a human action and
until now only the LLM could ask for it.

The channel is registered **before** the `syslog_client_connected` LLM call (a `manual`
routing rule can park that event for minutes). This client has no read loop at all — it is
fire-and-forget on both transports — so the commands are drained by a task of their own
(`command_loop`), registered with `AppState::register_client_task`. That task is now also
what keeps the transport alive after `connect_with_llm_actions` returns.

`send_syslog_message` yields `ClientActionResult::Custom` and the transport is either a
datagram socket or a stream, so there is no single write half for
`command_support::handle_stream_client_command` to use; the loop body is bespoke but applies
the action through `apply_action_result`, the same function the connected-event path uses, so
the RFC 5424 framing exists exactly once.

Outcomes:

| Action | Outcome |
|---|---|
| `send_syslog_message` (UDP) | `Sent { bytes_sent }` — the count `send_to` returned |
| `send_syslog_message` (TCP) | `Sent { bytes_sent }` — message length **plus 1** for the framing newline, written and flushed |
| `disconnect` | `Disconnected`; TCP is half-closed so the collector sees EOF, and the command loop ends |
| unknown type / bad params | `Rejected { error }` from `execute_action` |
