# XMPP Client Implementation

## Overview

XMPP (Extensible Messaging and Presence Protocol), formerly known as Jabber, is an open-standard instant messaging
protocol. This client implementation allows NetGet to connect to XMPP servers, send/receive messages, manage presence,
and interact with other XMPP clients.

**Complexity:** Hard (🟠)
**Status:** Experimental - Full implementation complete

## ✅ Implementation Status

**This implementation is complete and compiles successfully** with tokio-xmpp 5.0 and xmpp-parsers 0.22.

### What's Complete:

- ✅ Full tokio-xmpp 5.0 API integration
- ✅ Protocol structure and Client trait implementation
- ✅ Event type definitions (connected, message_received, presence_received)
- ✅ Action definitions (send_message, send_presence, disconnect)
- ✅ State machine (Idle/Processing/Accumulating)
- ✅ LLM integration with call_llm_for_client
- ✅ Bidirectional stanza handling (send and receive)
- ✅ Proper JID and Lang type handling for xmpp-parsers 0.22
- ✅ Channel-based architecture for concurrent send/receive
- ✅ Test infrastructure
- ✅ Documentation
- ✅ Feature flags and dependencies

## Library Choice

**Primary:** `tokio-xmpp` v5.0 + `xmpp-parsers`

### Why tokio-xmpp?

- **Async/Await Support:** Full tokio integration for non-blocking I/O
- **Modern Design:** Built for async Rust with proper error handling
- **Active Development:** Well-maintained with recent updates
- **XMPP Compliance:** Supports core XMPP RFCs (RFC 6120, 6121, 6122)
- **Parser Integration:** Works with `xmpp-parsers` for stanza construction

### Alternatives Considered

- `xmpp-rs`: Less mature, fewer features
- Raw XML parsing: Too complex, reinventing the wheel

## Architecture

### Connection Flow

```
1. Parse JID and password from remote_addr or startup params
   Format: "user@domain@password" or via startup params

2. Create tokio-xmpp Client with JID and password

3. Authenticate via SASL (handled by library)

4. Call LLM with "xmpp_connected" event

5. Spawn event loop to process incoming stanzas

6. State machine: Idle → Processing → Accumulating
   (prevents concurrent LLM calls)
```

### State Management

**Connection State:**

- **Idle:** Ready to process events
- **Processing:** LLM is currently processing an event
- **Accumulating:** Queuing events while LLM is busy

**Per-Client Data:**

- `state`: Current connection state
- `queued_events`: Events queued during Processing state
- `memory`: LLM conversation memory

**Stored in AppState:**

- `jid`: Connected Jabber ID
- XMPP client writer (for sending stanzas)

### XMPP Features Implemented

**✅ Implemented:**

1. **Authentication:** SASL authentication via tokio-xmpp
2. **Messages:** Send and receive chat messages
3. **Presence:** Send presence updates (away, chat, dnd, xa), receive presence from contacts
4. **Auto-reconnect:** Handled by tokio-xmpp

**⚠️ Partially Implemented:**

5. **IQ Stanzas:** Received but not yet processed (TODO)

**❌ Not Implemented:**

6. **Roster Management:** Add/remove contacts (future enhancement)
7. **Multi-User Chat (MUC):** Join/leave chatrooms (future enhancement)
8. **File Transfer:** XEP-0096/XEP-0234 (complex, low priority)
9. **Service Discovery:** XEP-0030 (future enhancement)
10. **Message Receipts:** XEP-0184 (future enhancement)

## LLM Integration

### Events Sent to LLM

1. **xmpp_connected**
    - Triggered: After successful authentication
    - Parameters: `jid` (connected Jabber ID)
    - LLM Action: Send initial presence, greet contacts, etc.

2. **xmpp_message_received**
    - Triggered: When receiving a message from another user
    - Parameters: `from`, `to`, `body`, `message_type`
    - LLM Action: Respond to message, log, ignore, etc.

3. **xmpp_presence_received**
    - Triggered: When receiving presence updates from contacts
    - Parameters: `from`, `presence_type`, `show`, `status`
    - LLM Action: Acknowledge, send message, update roster, etc.

### Actions Available to LLM

**Async Actions (user-triggered):**

- `send_message(to, body)` - Send message to a JID
- `send_presence(show?, status?)` - Update presence
- `disconnect()` - Disconnect from server

**Sync Actions (event-triggered):**

- `send_message(to, body)` - Reply to received message
- `wait_for_more()` - Accumulate more events before responding

### Action Execution

Actions return `ClientActionResult::Custom` with structured data:

```rust
ClientActionResult::Custom {
    name: "send_message",
    data: json!({
        "to": "friend@example.com",
        "body": "Hello!"
    })
}
```

The client handler parses the custom action and executes the corresponding XMPP stanza send.

## Message Flow Examples

### Example 1: Auto-Reply Bot

**Instruction:** "Auto-reply to all messages with 'I'm busy'"

**Flow:**

1. User sends message → `xmpp_message_received` event
2. LLM receives: `{"from": "alice@example.com", "body": "Hi there!"}`
3. LLM action: `send_message(to: "alice@example.com", body: "I'm busy")`
4. Client sends XMPP message stanza

### Example 2: Presence Monitor

**Instruction:** "Log when contacts come online or go offline"

**Flow:**

1. Contact changes presence → `xmpp_presence_received` event
2. LLM receives: `{"from": "bob@example.com", "presence_type": "Available", "show": "chat"}`
3. LLM action: Log to memory (no stanza sent)

## Known Limitations

### 1. No Roster Management

**Issue:** Cannot add/remove contacts programmatically
**Workaround:** Manually add contacts using XMPP client before connecting NetGet
**Future:** Implement roster management actions

### 2. IQ Stanzas Not Handled

**Issue:** IQ (Info/Query) stanzas are received but ignored
**Impact:** Cannot respond to service discovery, version queries, etc.
**Future:** Parse and respond to common IQ queries

### 3. No MUC (Multi-User Chat)

**Issue:** Cannot join group chats
**Workaround:** Use direct messages only
**Future:** Implement XEP-0045 for MUC support

### 4. No TLS Configuration

**Issue:** TLS settings are library defaults, no customization
**Impact:** Cannot connect to servers with self-signed certs
**Future:** Expose TLS configuration in startup params

### 5. Password in URL

**Issue:** Password must be in connection string or startup params
**Security:** Not ideal for production use
**Workaround:** Use startup params instead of URL

Until September 2026 that workaround did not exist. `jid` and `password` were declared
startup parameters, but `parse_connection_info` read them off
`ClientInstance::protocol_data` — which `cli/client_startup.rs` leaves as `Value::Null`,
and which this client writes to only *after* it has connected. Both were therefore always
`None`, every connection fell through to parsing `remote_addr` as `user@domain@password`,
and a caller who used the declared parameters instead was refused with "Invalid XMPP
address format". They are now read from `ConnectContext::startup_params`, which is where
what the caller actually passed arrives; `protocol_data` is consulted only as a second
chance, and either source may supply one half with `remote_addr` filling in the other.
Pinned by `tests/client/xmpp/startup_params_test.rs`.

## Connection String Format

**Option 1: URL Format**

```
user@domain@password
```

Example:

```
alice@example.com@secretpass
```

**Option 2: Startup Parameters (Recommended)**

```bash
open_client xmpp example.com --param jid=alice@example.com --param password=secretpass "Reply to all messages"
```

## Security Considerations

1. **TLS Encryption:** tokio-xmpp uses TLS by default (STARTTLS or direct TLS)
2. **Password Storage:** The password is taken from the startup parameters and moved
   straight into `tokio_xmpp::Client`; it is never written to `protocol_data`. Only the
   connected `jid` is recorded there. A password embedded in `remote_addr` is a different
   matter — `remote_addr` is shown in the dashboard and the status stream, which is the
   reason to prefer the startup parameters
3. **SASL Authentication:** Uses library's SASL implementation (PLAIN, SCRAM)

**⚠️ Warning:** Do not hardcode passwords in prompts or instructions. Use startup params.

## Testing Approach

### Local XMPP Server

**Option 1: Prosody (Recommended)**

```bash
# Install prosody
sudo apt install prosody

# Configure /etc/prosody/prosody.cfg.lua
# Add test user
sudo prosodyctl adduser alice@localhost

# Start server
sudo systemctl start prosody
```

**Option 2: ejabberd**

```bash
# Install ejabberd
sudo apt install ejabberd

# Add test user
sudo ejabberdctl register alice localhost password
```

### Public Test Server

XMPP has public test servers (e.g., `jabber.org`, `404.city`), but use with caution for testing.

### E2E Test Strategy

1. **Setup:** Start local prosody server with two test users
2. **Test 1:** Connect and send presence
3. **Test 2:** Send message to another JID
4. **Test 3:** Receive message and auto-reply
5. **Cleanup:** Disconnect

**LLM Call Budget:** < 10 calls

- 1 call for connect
- 3 calls for message send/receive
- 2 calls for presence updates

## Dependencies

```toml
tokio-xmpp = "5.0"
xmpp-parsers = "0.20"
```

**Transitive Dependencies:**

- `tokio-tls` (TLS support)
- `minidom` (XML DOM)
- `trust-dns-resolver` (SRV record lookups)

## Required API Updates for tokio-xmpp 5.0

The following changes are needed to make this implementation compile with tokio-xmpp 5.0:

### 1. JID Type Mismatch

**Issue:** `xmpp_parsers::Jid` != `tokio_xmpp::Jid`
**Fix:** Use `tokio_xmpp::Jid` throughout, as tokio-xmpp 5.0 re-exports xmpp_parsers types

```rust
// Instead of:
use xmpp_parsers::Jid;

// Use:
use tokio_xmpp::Jid;
```

### 2. Client Clone Not Supported

**Issue:** `XmppClient` no longer implements `Clone`
**Fix:** Restructure to use channels or split reading/writing differently

### 3. Missing Methods

**Issues:**

- `set_reconnect()` no longer exists
- `wait_for_event()` replaced with different API
- Stanza doesn't implement `Clone`

**Fix:** Review tokio-xmpp 5.0 API and use:

- Event stream API (likely `futures::Stream`)
- Remove clone calls on stanzas

### 4. Stanza Conversion

**Issue:** `message.into()` fails for type conversion
**Fix:** Use tokio-xmpp's re-exported types:

```rust
// Instead of:
use xmpp_parsers::message::Message;
let stanza: tokio_xmpp::Stanza = message.into();  // FAILS

// Use:
use tokio_xmpp::xmpp_parsers::message::Message;
let stanza = tokio_xmpp::Stanza::from(message);  // OK
```

### 5. Event Loop Pattern

The event loop needs to be rewritten to match tokio-xmpp 5.0's async stream pattern instead of `wait_for_event()`.

## Future Enhancements

1. **Roster Management:** Add/remove/list contacts
2. **MUC Support:** Join/leave chatrooms
3. **File Transfer:** Send/receive files (XEP-0096, XEP-0234)
4. **Message Receipts:** XEP-0184 for delivery confirmation
5. **Service Discovery:** XEP-0030 for capability negotiation
6. **PubSub:** XEP-0060 for publish-subscribe
7. **Message Archiving:** XEP-0136 for history retrieval
8. **OMEMO Encryption:** End-to-end encryption support

## References

- [RFC 6120](https://www.rfc-editor.org/rfc/rfc6120.html) - XMPP Core
- [RFC 6121](https://www.rfc-editor.org/rfc/rfc6121.html) - XMPP IM
- [RFC 6122](https://www.rfc-editor.org/rfc/rfc6122.html) - XMPP Address Format
- [tokio-xmpp Documentation](https://docs.rs/tokio-xmpp/)
- [xmpp-parsers Documentation](https://docs.rs/xmpp-parsers/)
- [XMPP Extensions (XEPs)](https://xmpp.org/extensions/)

## Command channel — the dashboard's `[ send ]`

Adopted. `tokio_xmpp::Client` is not clonable and is owned by the event loop task, so the command
loop reaches the stream the way every other producer here does — through the stanza channel — but
each request now carries an optional ack:

```rust
struct StanzaRequest { stanza: Stanza, ack: Option<oneshot::Sender<Result<(), String>>> }
```

`Client::send_stanza` resolves only once the stanza **has been written to the XMPP transport**,
and the event loop sends that result back on the ack. So an injected send reports what really
happened rather than "handed to a channel". The LLM path passes `ack: None` — it has no caller to
answer and must not serialise behind the loop's next turn.

Two structural changes came with it:

- The channel is registered before anything else, and the `xmpp_connected` LLM call now runs in
  **its own task**. Inline, that call blocked `connect()` itself and — worse for this feature —
  nothing was draining the stanza channel while it waited, so an injected stanza could not have
  been written until it finished.
- An injected `disconnect` signals a `oneshot` the event loop selects on; the loop breaks, drops
  the handle, sets the status and then closes the stream (bounded by a 5s timeout, because
  `close()` waits on a peer that may never have existed).

| Outcome | When |
|---|---|
| `Executed { detail }` | the stanza reached the transport (`<message/> to a@b (5 byte body) written to the XMPP transport`), or the action puts nothing on the wire (`wait_for_more`) |
| `Rejected { error }` | `execute_action` refused it: unknown name, missing `to`/`body` |
| `Disconnected` | `disconnect` |
| `Err(..)` | `send_stanza` failed, or the event loop ended before the stanza could be written |

**There is deliberately no `Sent { bytes_sent }`**: tokio-xmpp serialises and writes the stanza
internally and reports no byte count.

**Gap.** `tests/client/xmpp/command_channel_test.rs` pins registration, execution, the access-log
entry and disconnect, but **not** a stanza reaching a peer: no XMPP server this suite can start
completes tokio-xmpp's STARTTLS/SASL negotiation, so the wire path is still only exercised by the
`#[ignore]`d e2e test against a real prosody/ejabberd.
