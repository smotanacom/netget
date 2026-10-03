# WebRTC Client Testing

## Overview

Testing strategy for WebRTC client implementation. WebRTC presents unique testing challenges due to peer-to-peer nature
and signaling requirements.

## Test Categories

### 1. Unit Tests (No Ollama)

**File**: `tests/client/webrtc/e2e_test.rs` (non-E2E tests)

**Coverage**:

- Protocol registration verification
- Client state management
- Action parsing
- Event type definitions

**LLM Call Budget**: 0 (no Ollama required)
**Runtime**: < 1 second

**Example**:

```bash
./cargo-isolated.sh test --no-default-features --features webrtc \
  --test client::webrtc::e2e_test test_webrtc_protocol_registered
```

### 2. E2E Tests (Ollama Required)

**File**: `tests/client/webrtc/e2e_test.rs`

**Coverage**:

- Client initialization
- SDP offer generation
- Connection state management

**LLM Call Budget**: < 5 calls

- 1 call for initial connection event
- No additional calls without peer

**Runtime**: ~10 seconds

**Limitations**:

- Cannot complete full connection without peer
- No message exchange testing (requires peer)
- SDP answer application not tested (requires peer)

**Example**:

```bash
./cargo-isolated.sh test --no-default-features --features webrtc \
  --test client::webrtc::e2e_test test_webrtc_client_offer_generation -- --ignored
```

## Testing Challenges

### Challenge 1: Peer Requirement

**Problem**: WebRTC requires two peers to establish connection

**Solutions**:

1. **Manual Testing**: Use web browser as peer
    - Open https://webrtc.github.io/samples/src/content/datachannel/basic/
    - Exchange SDP with NetGet client
    - Test message exchange manually

2. **Dual NetGet Instances**: Run two NetGet instances
    - Instance A generates offer
    - Instance B generates answer
    - Exchange SDPs between instances
    - Test P2P messaging

3. **Mock Peer** (future): Implement test peer
    - Automated SDP exchange
    - Simulate data channel messages
    - Enable automated E2E tests

### Challenge 2: Signaling

**Problem**: Manual SDP exchange required

**Current Approach**:

- Test only offer generation
- Skip answer application in automated tests
- Manual testing for full connection

**Future Improvement**:

- WebSocket signaling server for tests
- Loopback connections
- Test peer with automated exchange

### Challenge 3: Timing

**Problem**: ICE gathering takes time (2-5 seconds)

**Approach**:

- Use `gathering_complete_promise()` to wait
- 10-second timeout in tests
- Verify SDP offer existence, not content

## Test Scenarios

### Scenario 1: Basic Initialization ✅

**Automated**: Yes
**LLM Calls**: 1-2

**Steps**:

1. Create WebRTC client
2. Verify SDP offer generated
3. Check protocol_data fields

**Expected**:

- Client status: Connecting
- SDP offer stored in protocol_data
- Offer contains ICE candidates

### Scenario 2: Connection Establishment ⚠️

**Automated**: No (requires peer)
**LLM Calls**: 3-4

**Steps**:

1. Generate SDP offer
2. Exchange with peer (manual)
3. Apply SDP answer
4. Wait for data channel open
5. Verify `webrtc_connected` event

**Expected**:

- Client status: Connected
- Data channel ready
- LLM receives connected event

### Scenario 3: Message Exchange ⚠️

**Automated**: No (requires peer)
**LLM Calls**: 5-8

**Steps**:

1. Establish connection (see Scenario 2)
2. Send message via LLM action
3. Peer receives message
4. Peer sends reply
5. LLM processes `webrtc_message_received` event
6. LLM sends response

**Expected**:

- Messages delivered reliably
- LLM responds to peer messages
- State machine handles queueing

### Scenario 4: Disconnection ✅

**Automated**: Partial
**LLM Calls**: 2

**Steps**:

1. Create client
2. Trigger disconnect action
3. Verify cleanup

**Expected**:

- Client removed from state
- Pointers cleaned up
- No memory leaks

## Manual Testing Guide

### Setup

1. **Install NetGet**:
   ```bash
   ./cargo-isolated.sh build --no-default-features --features webrtc
   ```

2. **Open Browser Peer**:
    - Navigate to: https://webrtc.github.io/samples/src/content/datachannel/basic/
    - Or use: https://appr.tc/ (Google WebRTC demo)

3. **Run NetGet**:
   ```bash
   ./target-claude/*/release/netget
   ```

### Test Procedure

1. **Create WebRTC Client**:
   ```
   > You are a helpful assistant. Open a WebRTC client to peer and send hello message
   ```

2. **Copy SDP Offer**:
    - NetGet displays SDP offer JSON
    - Copy entire offer block

3. **Paste in Browser**:
    - If using samples: Paste in "Remote peer's answer" field
    - If using appr.tc: Enter room code and paste offer

4. **Copy SDP Answer**:
    - Browser generates answer
    - Copy answer SDP

5. **Apply Answer in NetGet**:
    - Paste answer when prompted
    - LLM should generate `apply_answer` action

6. **Send Messages**:
   ```
   > Send message "Hello from NetGet!"
   ```

7. **Verify Exchange**:
    - Browser should receive message
    - Send reply from browser
    - NetGet LLM should see `webrtc_message_received` event
    - LLM generates response

### Expected Results

- ✅ SDP offer generated successfully
- ✅ Browser accepts offer
- ✅ Answer applied without errors
- ✅ Data channel opens
- ✅ Messages sent from NetGet appear in browser
- ✅ Messages from browser trigger LLM events
- ✅ LLM responds appropriately

## Known Issues

1. **No Automated Peer**: Full E2E requires manual testing
2. **Timeout Sensitivity**: ICE gathering may timeout on slow networks
3. **NAT Complexity**: Symmetric NAT may prevent connection without TURN
4. **Single Channel**: Only one data channel tested

## Future Test Improvements

1. **Test Peer Implementation**:
   ```rust
   // Minimal WebRTC peer for automated testing
   struct TestPeer {
       // Accepts offers, generates answers
       // Sends/receives messages
       // Validates behavior
   }
   ```

2. **Loopback Connections**:
    - Two WebRTC clients in same process
    - Automated SDP exchange
    - Full message flow testing

3. **Mock Data Channel**:
    - Stub out webrtc-rs for unit tests
    - Test LLM integration without real connection
    - Faster test execution

4. **Signaling Server**:
    - WebSocket server for SDP exchange
    - Enable multi-instance testing
    - Automated E2E flows

## Performance Targets

- **Offer Generation**: < 5 seconds
- **Connection Establishment**: < 10 seconds (with peer)
- **Message Latency**: < 100ms (P2P)
- **LLM Response Time**: < 2 seconds (with qwen3-coder:30b)

## Running Tests

```bash
# Unit tests only (no Ollama)
./cargo-isolated.sh test --no-default-features --features webrtc \
  --test client::webrtc::e2e_test test_webrtc_protocol_registered

# E2E tests (requires Ollama)
./cargo-isolated.sh test --no-default-features --features webrtc \
  --test client::webrtc::e2e_test test_webrtc_client_offer_generation -- --ignored

# All tests (LLM budget: < 5 calls)
./cargo-isolated.sh test --no-default-features --features webrtc \
  --test client::webrtc::e2e_test
```

## Test Maintenance

- **Update on API Changes**: Verify tests after webrtc-rs upgrades
- **Validate Manual Tests**: Run manual test procedure quarterly
- **Monitor LLM Budget**: Ensure < 10 calls total
- **Check Timeouts**: Adjust for network conditions

## References

- [WebRTC Testing Best Practices](https://webrtc.org/getting-started/testing)
- [webrtc-rs Examples](https://github.com/webrtc-rs/webrtc/tree/master/examples)
- [WebRTC Samples](https://webrtc.github.io/samples/)

---

## `command_channel_test.rs` — the dashboard's `[ send ]`

The one test in this directory that is **not** `#[ignore]`d, because it needs neither a second
peer nor Ollama. It uses `"ice_servers": []` so ICE gathers host candidates only and nothing
leaves the machine — the default Google STUN server would otherwise be contacted during
`connect()`.

**LLM calls: 0** (the client's LLM URL is `http://127.0.0.1:1`).

Asserts: the command handle exists before anything is sent; `create_channel` really creates a
channel on the live `RTCPeerConnection` and returns `Executed`; a send to an unknown channel
label is `Rejected`; a send on the default channel, which no peer ever opened, returns an
**error naming the channel** rather than a fake `Sent`; an unknown action is `Rejected`; the
injection is in the client's access log; `disconnect` returns `Disconnected` and drops the handle.

What it cannot cover: `Sent { bytes_sent }`, which needs an open data channel and therefore a
real second peer.
