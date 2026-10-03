# XMPP Client E2E Tests

## Overview

End-to-end tests for the XMPP (Jabber) client implementation. These tests verify that NetGet can successfully connect to
XMPP servers, send messages, update presence, and receive events.

## Test Strategy

**Approach:** Manual verification with local XMPP server
**LLM Call Budget:** < 10 calls per test suite
**Expected Runtime:** 15-30 seconds (excluding server setup)

## Test Categories

### 1. Connection Tests

- **Test:** `test_xmpp_client_connect`
- **Purpose:** Verify basic connection and authentication
- **LLM Calls:** 1 (on connect)
- **Setup:** Local XMPP server with test account
- **Validation:** Client status is Connected

### 2. Message Tests

- **Test:** `test_xmpp_client_send_message`
- **Purpose:** Verify sending messages to another JID
- **LLM Calls:** 2 (connect + message action)
- **Setup:** Two test accounts on local server
- **Validation:** Manual verification on receiving client

### 3. Presence Tests

- **Test:** `test_xmpp_client_presence`
- **Purpose:** Verify presence updates
- **LLM Calls:** 2 (connect + presence action)
- **Setup:** Local server, verify with another XMPP client
- **Validation:** Manual verification of presence status

## Test Setup

### Local XMPP Server (Prosody - Recommended)

**Installation:**

```bash
# Ubuntu/Debian
sudo apt install prosody

# macOS
brew install prosody
```

**Configuration** (`/etc/prosody/prosody.cfg.lua`):

```lua
VirtualHost "localhost"

-- Enable required modules
modules_enabled = {
    "roster"; -- Contact list
    "saslauth"; -- Authentication
    "tls"; -- TLS support
    "dialback"; -- Server-to-server communication
    "disco"; -- Service discovery
    "carbons"; -- Message carbons
    "ping"; -- Keepalive
}

-- Allow insecure connections for testing
c2s_require_encryption = false
s2s_require_encryption = false

-- Authentication
authentication = "internal_plain"

-- Logging
log = {
    info = "/var/log/prosody/prosody.log";
    error = "/var/log/prosody/prosody.err";
}
```

**Create Test Accounts:**

```bash
# Create alice@localhost
sudo prosodyctl adduser alice@localhost
# Enter password: password

# Create bob@localhost (for message tests)
sudo prosodyctl adduser bob@localhost
# Enter password: password
```

**Start Server:**

```bash
sudo systemctl start prosody
# Or
sudo prosodyctl start
```

**Verify Server:**

```bash
# Check if running on port 5222
netstat -ln | grep 5222

# Check logs
tail -f /var/log/prosody/prosody.log
```

### Alternative: ejabberd

**Installation:**

```bash
sudo apt install ejabberd
```

**Create Test Accounts:**

```bash
sudo ejabberdctl register alice localhost password
sudo ejabberdctl register bob localhost password
```

**Start Server:**

```bash
sudo systemctl start ejabberd
```

## Running Tests

### All Tests (Ignored by Default)

```bash
./cargo-isolated.sh test --no-default-features --features xmpp --test client::xmpp::e2e_test -- --ignored
```

### Specific Test

```bash
./cargo-isolated.sh test --no-default-features --features xmpp test_xmpp_client_connect -- --ignored
```

### With Logging

```bash
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features xmpp --test client::xmpp::e2e_test -- --ignored --nocapture
```

## Manual Verification

### Using Another XMPP Client

**Desktop Clients:**

- **Gajim** (Linux): `sudo apt install gajim`
- **Psi** (Cross-platform): https://psi-im.org/
- **Swift** (Cross-platform): https://swift.im/

**Console Client (for quick testing):**

```bash
# Install profanity
sudo apt install profanity

# Connect as bob
profanity
/connect bob@localhost
# Enter password: password

# Check for messages from alice
/msg alice@localhost
```

### Monitoring Server Logs

```bash
# Prosody
tail -f /var/log/prosody/prosody.log

# ejabberd
tail -f /var/log/ejabberd/ejabberd.log
```

## LLM Call Budget

**Total Budget:** < 10 LLM calls

**Breakdown:**

- Connection test: 1 call (connect event)
- Message test: 2 calls (connect + send message)
- Presence test: 2 calls (connect + send presence)
- **Total:** 5 calls minimum

**Actual Usage:** Depends on LLM instruction complexity and event processing

## Known Issues

### 1. Authentication Failures

**Symptom:** Connection fails with SASL error
**Cause:** Incorrect password or account doesn't exist
**Fix:** Recreate test account with correct password

### 2. TLS Errors

**Symptom:** TLS handshake failed
**Cause:** Server requires TLS but client doesn't support it
**Fix:** Disable TLS requirement in server config for testing

### 3. Connection Timeout

**Symptom:** Test times out after 10 seconds
**Cause:** Server not running or firewall blocking port 5222
**Fix:** Check server status and firewall rules

### 4. Message Not Received

**Symptom:** Message sent but not received by bob@localhost
**Cause:** bob is not online or server isn't routing messages
**Fix:** Ensure receiving client is connected and check server logs

### 5. Roster Not Loading

**Symptom:** Cannot see contacts
**Cause:** Roster management not implemented yet
**Fix:** This is a known limitation, manually add contacts via server admin

## Test Improvements (Future)

1. **Automated Verification:** Run two clients and verify message delivery
2. **Public Test Server:** Use `test.xmpp.jp` or similar for CI/CD
3. **Docker Compose:** Automated server setup with pre-configured accounts
4. **IQ Stanza Tests:** Add tests for service discovery (XEP-0030)
5. **MUC Tests:** Add multi-user chat tests when implemented
6. **Message Receipt Tests:** Verify XEP-0184 support when implemented

## CI/CD Considerations

**Current Status:** Tests are ignored by default (require local server)

**Options for CI:**

1. **Docker Prosody:** Run server in container, connect in tests
2. **Public Test Server:** Use `test.xmpp.jp` (requires internet)
3. **Mock Server:** Implement simple XMPP mock for testing

**Recommendation:** Use Docker Prosody for CI with pre-configured accounts.

## References

- [Prosody Documentation](https://prosody.im/doc/)
- [ejabberd Documentation](https://docs.ejabberd.im/)
- [XMPP Standards](https://xmpp.org/rfcs/)
- [XEP List](https://xmpp.org/extensions/) (XMPP Extension Protocols)

---

## `command_channel_test.rs` — the dashboard's `[ send ]`

The one test here that runs without a server, and it is deliberately narrow. It creates a client
for `netget@127.0.0.1@secret` — loopback, nothing listening — which is exactly the state in which
`[ send ]` must still be reachable rather than absent, and asserts: the command handle exists
before anything is sent (registration precedes the `xmpp_connected` LLM call, which now runs in
its own task); `wait_for_more` returns `Executed` saying nothing went on the wire; an unknown
action is `Rejected`; the injection is recorded in the client's access log; `disconnect` returns
`Disconnected` and drops the handle.

**LLM calls: 0** (the client's LLM URL is `http://127.0.0.1:1`).

What it cannot cover: a stanza actually reaching a peer. `Client::send_stanza` resolves only once
the stanza has been written to the transport, and no XMPP server this suite can start completes
tokio-xmpp's STARTTLS/SASL negotiation — so `send_message` remains covered only by the
`#[ignore]`d test above, against a real prosody/ejabberd.
