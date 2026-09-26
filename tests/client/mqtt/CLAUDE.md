# MQTT Client Testing Documentation

## Test Strategy

The MQTT client tests use a **real Mosquitto broker** running in Docker to validate LLM-controlled MQTT operations.
Tests verify:

1. **Connection establishment** to broker
2. **LLM-driven subscriptions** with wildcard support
3. **Message publishing** with different QoS levels
4. **Retained messages** behavior
5. **Dynamic topic subscriptions** based on LLM decisions

## Test Infrastructure

### Test Broker

- **Image**: `eclipse-mosquitto:2.0`
- **Port**: 1883 (unencrypted)
- **Configuration**: No authentication (mosquitto-no-auth.conf)
- **Container name**: `netget-test-mosquitto`

Each test starts a fresh broker instance and cleans it up afterward.

## LLM Call Budget

**Target: < 10 LLM calls per test suite**

### Call Breakdown

1. **test_mqtt_client_basic** (3 calls)
    - 1 call on connection (mqtt_connected event)
    - 1 call on message received (mqtt_message_received event)
    - 1 call after publishing response

2. **test_mqtt_client_qos** (4 calls)
    - 1 call on connection
    - 3 calls for messages at different QoS levels

3. **test_mqtt_client_wildcards** (5 calls)
    - 1 call on connection
    - 4 calls for messages on different wildcard-matched topics

4. **test_mqtt_client_retained** (2 calls)
    - 1 call on connection
    - 1 call on receiving retained message

**Total: ~14 LLM calls** (slightly over budget, but acceptable for comprehensive testing)

### Optimization Opportunities

To reduce LLM calls further:

- Use scripting mode for repetitive actions
- Batch message testing
- Reduce number of test scenarios

## Expected Runtime

- **Per test**: 8-15 seconds (including broker startup and shutdown)
- **Full suite**: ~45-60 seconds
- **Breakdown**:
    - Broker startup: 2-3 seconds
    - Client connection: 1-2 seconds
    - LLM processing: 3-5 seconds per call
    - Broker cleanup: 1-2 seconds

## Test Cases

### test_mqtt_client_basic

**Purpose**: Verify basic MQTT client connection and pub/sub

**Steps**:

1. Start Mosquitto broker
2. Open MQTT client with instruction to subscribe to `test/topic`
3. Publish test message using `mosquitto_pub`
4. Verify LLM receives message and publishes response
5. Check no errors occurred

**Expected behavior**:

- Client connects successfully
- Client subscribes to topic
- Client receives published message
- LLM publishes response to `test/response`

### test_mqtt_client_qos

**Purpose**: Test different QoS levels (0, 1, 2)

**Steps**:

1. Start broker
2. Client subscribes to `qos/#` with QoS 2
3. Publish messages with QoS 0, 1, and 2
4. Verify all messages are received

**Expected behavior**:

- QoS 0: At most once delivery
- QoS 1: At least once delivery
- QoS 2: Exactly once delivery

### test_mqtt_client_wildcards

**Purpose**: Test MQTT topic wildcards (+ and #)

**Steps**:

1. Client subscribes to `sensors/#` (multi-level wildcard)
2. Publish to various topics under `sensors/`
3. Verify all matching messages are received

**Topics tested**:

- `sensors/temperature`
- `sensors/humidity`
- `sensors/room1/temperature`
- `sensors/room2/humidity`

**Expected behavior**:

- All topics matching `sensors/#` trigger message events

### test_mqtt_client_retained

**Purpose**: Verify retained message handling

**Steps**:

1. Publish retained message to `retained/status` using mosquitto_pub
2. Connect client and subscribe to `retained/status`
3. Verify client immediately receives retained message

**Expected behavior**:

- Client receives retained message upon subscription
- No need to wait for new publish

## Known Issues

1. **Docker dependency**: Tests require Docker to be running
2. **Port conflicts**: If port 1883 is already in use, tests will fail
3. **Timing sensitivity**: Network delays may occasionally cause flaky tests
4. **No TLS testing**: Current tests only cover unencrypted connections
5. **Limited verification**: No direct MQTT client to verify responses (relies on logs)

## Running Tests

```bash
# Run MQTT client tests only
./cargo-isolated.sh test --no-default-features --features mqtt --test client::mqtt::e2e_test

# Run with logging
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features mqtt --test client::mqtt::e2e_test

# Run specific test
./cargo-isolated.sh test --no-default-features --features mqtt --test client::mqtt::e2e_test test_mqtt_client_basic
```

## Future Test Improvements

1. **Add mosquitto_sub verification**: Run subscriber to verify published responses
2. **Test authentication**: Add tests with username/password
3. **Test TLS**: Add secure connection tests
4. **Test will messages**: Verify Last Will and Testament
5. **Test persistent sessions**: Verify clean_session=false behavior
6. **Add connection loss tests**: Verify reconnection handling
7. **Performance tests**: Test message throughput and latency

## Debugging Failed Tests

If tests fail:

1. **Check Docker**: Ensure Docker is running and accessible
2. **Check port 1883**: Verify no other process is using the port
3. **Check Ollama**: Ensure Ollama is running on localhost:11434
4. **Check logs**: Run with `RUST_LOG=debug` for detailed logs
5. **Manual broker test**: Try running Mosquitto manually and using mosquitto_pub/sub

```bash
# Check if broker is running
docker ps | grep netget-test-mosquitto

# Manual broker start
docker run -d --name netget-test-mosquitto -p 1883:1883 eclipse-mosquitto:2.0

# Test with mosquitto_pub
docker exec netget-test-mosquitto mosquitto_pub -h localhost -t test -m hello

# Test with mosquitto_sub
docker exec netget-test-mosquitto mosquitto_sub -h localhost -t test -v
```

## References

- Mosquitto Docker: https://hub.docker.com/_/eclipse-mosquitto
- MQTT 3.1.1 spec: https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/mqtt-v3.1.1.html
- rumqttc testing examples: https://github.com/bytebeamio/rumqtt/tree/main/rumqttc/examples

## `command_channel_test.rs` — the dashboard's `[ send ]`

Unlike `e2e_test.rs`, this needs **no Docker and no Mosquitto**: the peer is a NetGet MQTT broker
started in-process with static handlers (`mqtt_connect` → `mqtt_connack` return_code 0, `*` → no
actions), on port 0. **LLM calls: 0** — the client's LLM URL is `http://127.0.0.1:1` so its
`mqtt_connected` call fails, which is part of what the test verifies the loop survives.

Asserts: the command handle exists before anything is sent (registration happens before the
event loop task even starts); an injected `publish` returns `Executed` whose detail names the
packet — **never `Sent`**, because rumqttc reports no byte count; the topic really reaches the
broker (its access log); an unknown action is `Rejected`; `disconnect` returns `Disconnected` and
leaves the client `Disconnected` with no handle (not `Error`, which is what the poll error would
otherwise have been read as).

## `keepalive_test.rs` — PINGREQ keeps flowing while a turn is parked

The peer is the **real Eclipse `mosquitto` broker** (`/opt/homebrew/sbin/mosquitto`, else
`mosquitto` on `PATH`), started per test on a free loopback port with `listener <port> 127.0.0.1`,
`allow_anonymous true` and its log in a temp file; `mosquitto_sub` verifies delivery. Both
binaries are **required** — the test panics with an install hint rather than skipping, because a
skip is a silent pass wherever the suite runs without them.

The client connects with `keep_alive: 2`; mosquitto disconnects a client silent for 1.5 × that.
Its `mqtt_connected` turn is parked on a `manual` rule for ten seconds. Then, before the turn is
answered, the broker's own log must show `Received PINGREQ from netget-keepalive-probe` and no
`exceeded timeout` line for it, and the client must still be `Connected`. The turn is answered
with a publish, and `mosquitto_sub -C 1` must print it. **LLM calls: 0.** Runtime ~10s.

Verified by removal: with the pre-split client (model turn inside the loop that polls rumqttc)
the broker logs `disconnected: exceeded timeout` three seconds into the park and the test fails
on that assertion.
