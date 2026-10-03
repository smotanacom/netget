# mDNS Client E2E Tests

## Test Strategy

The mDNS client tests verify service discovery and hostname resolution functionality on local networks. Tests use the
actual NetGet binary in black-box mode, spawning mDNS clients and checking their behavior.

## Test Approach

**Black-box testing**: Spawn NetGet binary with mDNS client instructions, capture output, verify expected behavior.

**Challenge**: mDNS requires active mDNS responders on the network:

- **macOS**: Built-in mDNSResponder (Bonjour)
- **Linux**: Avahi daemon (if installed)
- **Windows**: Bonjour Print Services (if installed)

**Graceful degradation**: Tests verify client initialization and service discovery attempts, even if no services are
found.

## LLM Call Budget

**Total budget**: < 5 LLM calls per test suite

**Breakdown**:

- Test 1 (initialization): 1 LLM call (client startup)
- Test 2 (service discovery): 2 LLM calls (startup + browse)
- Test 3 (hostname resolution): 2 LLM calls (startup + resolve)

**Optimization**:

- Simple, focused test scenarios
- Short wait times (2-12 seconds)
- No complex multi-step interactions

## Expected Runtime

**Per test**: 3-15 seconds
**Total suite**: 30-45 seconds

**Timing breakdown**:

- Binary startup: 1-2 seconds
- mDNS client initialization: 0.5-1 second
- Service discovery: 2-10 seconds (depends on network)
- Hostname resolution: 1-3 seconds
- Cleanup: 0.5-1 second

## Test Cases

### 1. test_mdns_client_initialization

**Purpose**: Verify mDNS client can be initialized

**LLM calls**: 1
**Runtime**: ~3 seconds

**Approach**:

- Start NetGet with mDNS client
- Instruction: "Browse for HTTP services"
- Verify output shows initialization

**Expected output**:

- "mDNS client initialized"
- "ready for service discovery"

### 2. test_mdns_client_service_discovery

**Purpose**: Verify mDNS client can browse for services

**LLM calls**: 2
**Runtime**: ~15 seconds

**Approach**:

- Start NetGet with mDNS client
- Instruction: "Browse for _services._dns-sd._udp.local"
- Wait 10 seconds for discovery
- Verify client attempted browsing

**Expected output**:

- "Browsing for mDNS service"
- "service_type" or "service found" (if services present)
- May show "No services found" (acceptable)

**Note**: `_services._dns-sd._udp.local` is a meta-query that discovers all available service types on the network.

### 3. test_mdns_client_hostname_resolution

**Purpose**: Verify mDNS client can resolve .local hostnames

**LLM calls**: 2
**Runtime**: ~5 seconds

**Approach**:

- Start NetGet with mDNS client
- Instruction: "Resolve localhost.local"
- Verify client attempted resolution

**Expected output**:

- "Resolving hostname"
- "localhost.local" or IP address (if resolvable)
- May show resolution failure (acceptable on some systems)

## Known Issues

### 1. Network Dependency

**Issue**: Tests depend on network having active mDNS responders
**Impact**: Service discovery may not find services in minimal environments
**Mitigation**: Tests verify attempt to discover, not success

### 2. Platform Differences

**Issue**: mDNS availability varies by platform

- macOS: Always available (mDNSResponder)
- Linux: Requires Avahi daemon
- Windows: Requires Bonjour
  **Impact**: Tests may behave differently across platforms
  **Mitigation**: Graceful handling of "no services found"

### 3. Timing Variability

**Issue**: Service discovery timing depends on network latency
**Impact**: Tests may need longer wait times in some networks
**Current**: 10-second wait for service discovery
**Adjustment**: Increase if tests timeout frequently

### 4. Firewall/Security

**Issue**: Some environments block multicast UDP (224.0.0.251:5353)
**Impact**: mDNS may not work at all
**Mitigation**: Tests verify initialization, not necessarily discovery

## Flaky Tests

**None expected** - Tests are designed to verify attempts, not results.

**If flaky**:

- Increase wait times (2s → 5s for initialization, 10s → 20s for discovery)
- Check network firewall allows multicast UDP
- Verify mDNS responder is running (Avahi on Linux)

## Running Tests

### With Feature Flag

```bash
./cargo-isolated.sh test --no-default-features --features mdns --test client::mdns::e2e_test
```

### Prerequisites

- **Linux**: Install Avahi
  ```bash
  sudo apt-get install avahi-daemon
  sudo systemctl start avahi-daemon
  ```
- **macOS**: Built-in (no setup needed)
- **Windows**: Install Bonjour Print Services

### Debugging

```bash
# Check if mDNS responder is running
# Linux
systemctl status avahi-daemon

# Check for mDNS services on network
avahi-browse -a  # Linux
dns-sd -B _services._dns-sd._udp  # macOS
```

## Future Improvements

1. **Mock mDNS responder**: Create test fixture that advertises a fake service
2. **Conditional skip**: Skip tests if no mDNS responder detected
3. **Service registration test**: Once server supports registration
4. **TXT property parsing**: Verify client can extract service metadata

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

**This test asserts `Executed`, not `Sent`, and that is the point.** `mdns-sd` owns the
multicast socket and reports no byte count, so the client reports what the daemon was asked to
do rather than inventing one. The test pins that: the `detail` must name `browse_service` and
the service type. No mDNS responder is started — the assertion is on the outcome and the
access-log entry, not on a discovered service.
