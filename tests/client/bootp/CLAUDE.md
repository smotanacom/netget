# BOOTP Client E2E Test Documentation

## Test Strategy

The BOOTP client tests verify LLM-controlled BOOTP request/reply flows using a real BOOTP/DHCP server. Tests focus on:

1. Basic BOOTP request/reply cycle
2. Broadcast discovery
3. Custom MAC address handling
4. Error handling (no server)

## Test Environment

### BOOTP Server Setup

Tests require a BOOTP/DHCP server. Use `dnsmasq` for simple testing:

**Install dnsmasq (Ubuntu/Debian):**

```bash
sudo apt-get install dnsmasq
```

**Configure dnsmasq for BOOTP:**

```bash
# /etc/dnsmasq.conf
interface=lo
bind-interfaces
dhcp-range=127.0.0.2,127.0.0.254,255.255.255.0,12h
dhcp-boot=pxelinux.0
enable-tftp
tftp-root=/var/tftp
log-dhcp
```

**Start dnsmasq:**

```bash
sudo systemctl start dnsmasq
# Or for testing:
sudo dnsmasq --conf-file=/path/to/test-dnsmasq.conf --no-daemon
```

**Alternative: isc-dhcp-server**

```bash
sudo apt-get install isc-dhcp-server
```

Configure `/etc/dhcp/dhcpd.conf`:

```
subnet 127.0.0.0 netmask 255.255.255.0 {
  range 127.0.0.100 127.0.0.200;
  option routers 127.0.0.1;
  filename "pxelinux.0";
  next-server 127.0.0.1;
}
```

### Port Binding Requirements

BOOTP clients traditionally bind to UDP port 68, which may require elevated privileges:

**Option 1: Run tests with sudo**

```bash
sudo ./cargo-isolated.sh test --no-default-features --features bootp --test client::bootp::e2e_test
```

**Option 2: Grant CAP_NET_BIND_SERVICE capability**

```bash
# Allow binding to privileged ports without root
sudo setcap 'cap_net_bind_service=+ep' /path/to/netget
```

**Option 3: Use random port (fallback)**

- Tests automatically fall back to random port if 68 unavailable
- Works with some servers but may have compatibility issues

## LLM Call Budget

Total budget: **< 10 LLM calls** across all tests

### Per-Test Breakdown

| Test                             | LLM Calls | Description                                        |
|----------------------------------|-----------|----------------------------------------------------|
| `test_bootp_request_reply`       | 2         | Basic request/reply                                |
| `test_bootp_broadcast_discovery` | 2-3       | Broadcast discovery (may receive multiple replies) |
| `test_bootp_no_server`           | 1         | Error handling (no reply)                          |
| `test_bootp_custom_mac`          | 2         | Custom MAC address                                 |
| **Total**                        | **7-8**   | Well under budget                                  |

### LLM Call Flow

**Test: test_bootp_request_reply**

1. **Call 1 (bootp_connected):**
    - Event: Client ready
    - LLM Action: `send_bootp_request` with MAC

2. **Call 2 (bootp_reply_received):**
    - Event: Reply received (IP, server, boot file)
    - LLM Action: Analyze and disconnect

**Test: test_bootp_broadcast_discovery**

1. **Call 1 (bootp_connected):**
    - LLM Action: `send_bootp_request` with broadcast=true

2. **Call 2-3 (bootp_reply_received):**
    - LLM may receive multiple replies from different servers
    - LLM Action: `wait_for_more` or disconnect

## Test Execution

### Run All BOOTP Client Tests

```bash
# Build with BOOTP feature
./cargo-isolated.sh build --no-default-features --features bootp

# Run tests (requires BOOTP server running)
sudo ./cargo-isolated.sh test --no-default-features --features bootp --test client::bootp::e2e_test -- --nocapture

# Run specific test
sudo ./cargo-isolated.sh test --no-default-features --features bootp --test client::bootp::e2e_test test_bootp_request_reply -- --nocapture
```

### Expected Runtime

- **Per test:** 5-30 seconds (depending on server response time)
- **Total suite:** 1-2 minutes
- **Timeout:** 30 seconds per test (configurable)

## Test Descriptions

### 1. test_bootp_request_reply

**Purpose:** Verify basic BOOTP request/reply cycle

**Flow:**

1. Connect to BOOTP server at 127.0.0.1:67
2. LLM sends BOOTP request for MAC 00:11:22:33:44:55
3. Server replies with IP assignment and boot info
4. LLM processes reply and disconnects

**Expected Behavior:**

- Client connects successfully
- BOOTP request sent with specified MAC
- Reply received with assigned IP, server IP, boot filename
- LLM interprets reply correctly

**Verification:**

```rust
assert!(all_messages.contains("BOOTP client") && all_messages.contains("connected"));
assert!(all_messages.contains("BOOTP request sent"));
```

**Known Issues:**

- Requires BOOTP server running on 127.0.0.1:67
- May need sudo for port 68 binding

---

### 2. test_bootp_broadcast_discovery

**Purpose:** Test broadcast BOOTP discovery (may find multiple servers)

**Flow:**

1. Connect with broadcast address (255.255.255.255:67)
2. LLM sends broadcast BOOTP request
3. Multiple servers may respond
4. LLM waits for multiple replies or times out

**Expected Behavior:**

- Broadcast request sent
- May receive multiple replies (if multiple BOOTP servers on network)
- LLM handles multiple responses

**Verification:**

```rust
assert!(all_messages.contains("255.255.255.255") || all_messages.contains("broadcast"));
```

**Known Issues:**

- Broadcast may be blocked by firewall
- Results vary by network configuration

---

### 3. test_bootp_no_server

**Purpose:** Test error handling when no BOOTP server responds

**Flow:**

1. Connect to non-existent server (192.0.2.1:67)
2. LLM sends request
3. No reply received (timeout)
4. Client remains connected (UDP is stateless)

**Expected Behavior:**

- Client starts successfully (UDP allows blind sending)
- Request sent
- No reply received (expected)
- No crash or error

**Verification:**

```rust
assert!(result.is_ok(), "BOOTP client should start even without server");
assert!(all_messages.contains("connected") || all_messages.contains("BOOTP"));
```

**Notes:**

- Short timeout (5s) since no reply expected
- Verifies graceful handling of no-response scenario

---

### 4. test_bootp_custom_mac

**Purpose:** Verify LLM correctly handles custom MAC address

**Flow:**

1. Instruction specifies MAC AA:BB:CC:DD:EE:FF
2. LLM sends request with that MAC
3. Server replies (may assign specific IP based on MAC)
4. Verify MAC was used in request

**Expected Behavior:**

- LLM parses MAC from instruction
- Request includes specified MAC
- Server responds (if configured)

**Verification:**

```rust
assert!(all_messages.to_lowercase().contains("aa:bb:cc:dd:ee:ff"));
```

---

## Common Issues and Troubleshooting

### Issue: "Permission denied" (bind port 68)

**Cause:** Port 68 requires elevated privileges

**Solutions:**

1. Run with sudo: `sudo ./cargo-isolated.sh test ...`
2. Grant capability: `sudo setcap 'cap_net_bind_service=+ep' target/debug/netget`
3. Accept fallback to random port (tests will still pass)

**Verification:**

```bash
# Check if test used port 68 or fallback
grep "Failed to bind to port 68" test-output.log
```

---

### Issue: "No BOOTP reply received"

**Cause:** BOOTP server not running or misconfigured

**Solutions:**

1. Verify dnsmasq is running: `sudo systemctl status dnsmasq`
2. Check server logs: `sudo journalctl -u dnsmasq -f`
3. Test with dhcping: `sudo dhcping -s 127.0.0.1`

**Verification:**

```bash
# Test server manually
sudo dhcping -c 127.0.0.1 -s 127.0.0.1 -h 00:11:22:33:44:55
```

---

### Issue: "Broadcast blocked"

**Cause:** Firewall or network policy blocks broadcast

**Solutions:**

1. Disable firewall temporarily: `sudo ufw disable`
2. Allow DHCP/BOOTP: `sudo ufw allow 67/udp` and `sudo ufw allow 68/udp`
3. Use unicast to specific server

**Verification:**

```bash
# Check firewall rules
sudo iptables -L -n | grep -i dhcp
```

---

### Issue: Tests timeout

**Cause:** LLM taking too long or server not responding

**Solutions:**

1. Increase timeout in test code
2. Use faster LLM model (e.g., qwen3-coder:30b → qwen3-coder:8b)

**Verification:**

```bash
# Monitor Ollama performance
curl http://127.0.0.1:11434/api/ps
```

---

## Test Maintenance

### When to Update Tests

1. **LLM model changes:** Adjust expected behavior if model reasoning changes
2. **Protocol changes:** Update if BOOTP packet format is modified
3. **New features:** Add tests for new BOOTP actions or events
4. **Server updates:** Verify compatibility with new dnsmasq/isc-dhcp versions

### LLM Call Budget Monitoring

If tests exceed budget (> 10 calls):

1. Combine related tests
2. Use scripting mode for repetitive actions
3. Reduce timeout to fail faster
4. Skip redundant LLM calls (e.g., reuse connections)

**Current budget:** 7-8 calls (comfortable margin)

## References

- **RFC 951:** Bootstrap Protocol (BOOTP) - https://www.rfc-editor.org/rfc/rfc951
- **dnsmasq:** http://www.thekelleys.org.uk/dnsmasq/doc.html
- **ISC DHCP Server:** https://www.isc.org/dhcp/
- **dhcproto crate:** https://crates.io/crates/dhcproto

## Test Server Configuration Examples

### Minimal dnsmasq.conf for Testing

```bash
# test-dnsmasq.conf
port=0  # Disable DNS
interface=lo
bind-interfaces
dhcp-range=127.0.0.100,127.0.0.200,255.255.255.0,1h
dhcp-boot=pxelinux.0,bootserver,127.0.0.1
log-dhcp
log-queries
```

**Run:**

```bash
sudo dnsmasq --conf-file=test-dnsmasq.conf --no-daemon --log-facility=-
```

### ISC DHCP Server for Testing

```bash
# test-dhcpd.conf
default-lease-time 600;
max-lease-time 7200;

subnet 127.0.0.0 netmask 255.255.255.0 {
  range 127.0.0.100 127.0.0.200;
  option routers 127.0.0.1;
  option domain-name-servers 8.8.8.8;
  next-server 127.0.0.1;
  filename "pxelinux.0";
}
```

**Run:**

```bash
sudo dhcpd -f -d -cf test-dhcpd.conf lo
```

## Summary

- **Total tests:** 4
- **LLM budget:** 7-8 calls (< 10 target)
- **Runtime:** 1-2 minutes
- **Server requirement:** dnsmasq or isc-dhcp-server
- **Privilege requirement:** sudo for port 68 (or use fallback)
- **Test coverage:** Request/reply, broadcast, MAC handling, error cases

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

The receiver is a plain `tokio::net::UdpSocket` on `127.0.0.1:0`, not a NetGet BOOTP server:
a BOOTP server binds privileged port 67 and this suite must run unprivileged. A raw socket is
also the stricter assertion — the test compares the reported `Sent { bytes_sent }` against the
datagram length actually received, and checks the `op` byte and the `chaddr` it injected.
Asserted outcomes: `Rejected` for an unknown action, `Sent` for `send_bootp_request` with
`broadcast: false`, `Disconnected` for `disconnect`, and no command handle afterwards.
