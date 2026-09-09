# MQTT server tests

Everything below was checked against the files it describes. The version this replaced was
written while the broker was a placeholder and was never updated: it documented a "Current
Status: Placeholder tests only", a migration path through five phases of removing `#[ignore]`,
and a test named `test_mqtt_placeholder_registered` that does not exist. The broker has been a
full MQTT 3.1.1 implementation for a long time. Verify against the source, not against this
file.

## What runs

`tests/server/mqtt/mod.rs` declares three modules, all gated `#[cfg(all(test, feature = "mqtt"))]`.
Seven tests, none `#[ignore]`d, none able to skip.

| Test | File | What it proves |
|---|---|---|
| `test_mqtt_broker_starts` | `e2e_test.rs` | the `open_server` path binds a port |
| `test_mqtt_keyword_detection` | `e2e_test.rs` | every keyword `MqttProtocol::keywords()` advertises resolves to `MQTT` |
| `test_mqtt_basic_connect` | `e2e_test.rs` | **rumqttc** completes CONNECT → CONNACK |
| `test_mqtt_subscribe_and_receive_a_published_message` | `e2e_test.rs` | **rumqttc** completes CONNECT → SUBSCRIBE → SUBACK → PUBLISH and receives the broker's own PUBLISH back, topic and payload asserted |
| `test_mqtt_refuses_connect_when_llm_fails` | `llm_failure_test.rs` | CONNACK return code 3 and a close, not code 0 |
| `test_mqtt_refuses_subscribe_when_llm_fails` | `llm_failure_test.rs` | SUBACK 0x80 per filter |
| `injected_mqtt_publish_reaches_raw_socket_and_close_sends_eof` | `peer_inject_test.rs` | the dashboard's `[ message this peer ]` / `[ disconnect this peer ]` reach a live connection |

`test_mqtt_subscribe_and_receive_a_published_message` is the evidence behind the **Beta**
rating. `rumqttc` is an unconditional dev-dependency, so it either compiles and runs or the
suite does not build — there is no "is it installed?" gate and no `SKIP: … not installed`
branch to hide a silent pass behind.

`test_mqtt_keyword_detection` asks `ServerRegistry::parse_from_str` directly and does not spawn
NetGet. The version before it did, with no `.with_mock()`, so the LLM call always failed, no
server was ever started, and the harness returned `Expected exactly 1 server, got 0` — a string
containing neither "unknown" nor "Unknown", which was the whole assertion. It passed for any
prompt, including one naming no protocol at all, and its `else` branch would have failed the
test had the broker actually worked.

## The two mock traps this protocol sets

Both are the "rule never matches, failure surfaces two steps later" shape, and both are
encoded in `test_mqtt_subscribe_and_receive_a_published_message`:

- **`mqtt_suback` must echo `packet_id` from the event.** rumqttc blocks until a SUBACK carrying
  the id *it* chose arrives, so a hardcoded id stalls the subscribe forever. Use
  `respond_with_actions_from_event`, for the same reason the UDP-style protocols must.
- **The event `mqtt_publish` and the action `mqtt_publish` share a name and point in opposite
  directions.** The event is the client's PUBLISH arriving at the broker; the action is the
  broker sending one out. Answering the event with the action is what forwards the message.

A third: **acceptance has to be asked for.** A CONNECT whose handler cannot run is refused with
CONNACK 3, so a test with no `mqtt_connect` rule tests a broker that turns clients away. Both
rumqttc tests carry an explicit `mqtt_connect` rule for that reason.

## LLM call budget

Every subprocess test calls `wait_for_mocks(30)` then `verify_mocks()`.

| Test | Calls |
|---|---|
| `test_mqtt_broker_starts` | 1 (startup) |
| `test_mqtt_basic_connect` | 1 startup + 1 `mqtt_connect` |
| `test_mqtt_subscribe_and_receive_a_published_message` | 1 startup + 1 `mqtt_connect` + 1 `mqtt_subscribe` + 1 `mqtt_publish` |
| `test_mqtt_refuses_*` | 1 startup each; the per-packet call is *made to fail on purpose* |
| `test_mqtt_keyword_detection` | 0 — no subprocess |
| `injected_mqtt_publish_reaches_raw_socket_and_close_sends_eof` | 0 — static handlers, LLM points at an unreachable URL |

Roughly 10 across the suite, which is the target.

PINGREQ, PUBREL and the malformed-packet close cost **zero** LLM calls by construction —
`dispatch_packet` answers them in Rust. A keepalive that consulted the model would be a defect,
not a slow path: the client declares the connection dead while the call is in flight.

## Not covered

TLS (8883), WebSocket transport, MQTT v5, retained messages, wildcard subscription matching,
last-will delivery, session resume across reconnect, and keep-alive timeout enforcement — the
broker does not implement a subscription table, a retained-message store or keep-alive reaping,
so there is nothing to test. See `src/server/mqtt/CLAUDE.md` for why storage is absent.

## Running

```bash
export CARGO_TARGET_DIR=/Users/matus/dev/netget-shared-target
cargo test --no-default-features --features mqtt --test server -- server::mqtt --test-threads=100
```

`--test` names a *target*. `--test server::mqtt::e2e_test` lists targets and exits without
running anything.
