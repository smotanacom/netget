# MQTT Protocol Implementation

MQTT 3.1.1 broker. Control packets are parsed and built by hand in `mod.rs`; the LLM (or
a script/static handler) decides every acknowledgement and every message delivery.

**State**: **Beta** (August 2026) — works against a real third-party client. `rumqttc` drives
CONNECT/CONNACK in `test_mqtt_basic_connect`, and
`test_mqtt_subscribe_and_receive_a_published_message` takes it all the way through
CONNECT → SUBSCRIBE → SUBACK → PUBLISH and back to the broker's own PUBLISH arriving on the
same connection, with topic and payload asserted. Neither test is `#[ignore]`d. Also verified
with a raw-socket MQTT client driving QoS 0, 1 and 2, plus PINGREQ.

The promotion is *not* new implementation work — the broker already did all of this. What was
missing was evidence, and the reason it looked missing is worth recording: the root CLAUDE.md
listed mqtt as "`mqtt`, whose rumqttc tests are all `#[ignore]`d", and that was wrong twice
over. The four pub/sub tests were inside a `/* … */` block, so nothing compiled them and
`--include-ignored` could never have run them; and the rumqttc test that *does* run was never
ignored at all. Their `#[ignore = "MQTT broker not yet implemented"]` markers were left over
from the placeholder described below. The dead block is deleted and replaced by one real
pub/sub test.

Two things that will silently break a mock for this protocol, both of which this test
encodes: `mqtt_suback` **must** echo `packet_id` from the event (rumqttc blocks until a SUBACK
carrying the id it chose arrives, so a hardcoded id stalls the subscribe forever), and the
event `mqtt_publish` and the action `mqtt_publish` share a name while pointing in opposite
directions — the event is the client's PUBLISH arriving, the action is the broker sending one.
**Port**: 1883 by default. **Privilege**: `None` (1883 > 1024).
**Stack**: `ETH>IP>TCP>MQTT`. **Spec**: MQTT 3.1.1 (OASIS / ISO-IEC 20922).

This replaces a placeholder that returned empty vectors from `get_sync_actions()` and
`get_event_types()` while advertising itself as `Experimental`. It answered CONNECT with
a hardcoded CONNACK and PINGREQ with PINGRESP, ignored PUBLISH and SUBSCRIBE entirely,
never called the LLM, and `execute_action` errored on every input.

## Why hand-written rather than rumqttd

`rumqttd` is a dependency of the `mqtt` feature but is not used. It owns its own session,
subscription and routing model — precisely the part the LLM has to own here — and exposes
no hook for "ask something else whether this CONNECT is acceptable". MQTT 3.1.1 control
packets are a varint length, length-prefixed UTF-8 strings and a handful of fixed-size
acknowledgements, so the codec is ~200 lines and every byte is inspectable.

## What the model sees

| Event | Fired when | Must be answered with |
|---|---|---|
| `mqtt_connect` | CONNECT received | `mqtt_connack` |
| `mqtt_publish` | client published | `mqtt_puback` (QoS 1), `mqtt_pubrec` (QoS 2), nothing (QoS 0) |
| `mqtt_subscribe` | SUBSCRIBE received | `mqtt_suback` |
| `mqtt_unsubscribe` | UNSUBSCRIBE received | `mqtt_unsuback` |

`mqtt_connect` carries `client_id`, `username`, `has_password`, `clean_session`,
`keep_alive`, `protocol_name`, `protocol_level`, `will_topic`, `will_message`. The
password itself is not surfaced.

`mqtt_publish` carries `client_id`, `topic`, `payload`, `payload_is_text`,
`payload_size`, `qos`, `retain`, `duplicate`, `packet_id`, and `connected_clients` — the
list of client ids currently attached to this server, so the model can forward the
message without guessing names.

`mqtt_subscribe` carries `packet_id` and `topics`, an ordered list of
`{"filter": "...", "qos": N}`.

### Correlation identifiers

MQTT correlates by **packet identifier**, and every acknowledgement must echo the one
from the request or the client retries forever.

- `packet_id` is exposed on `mqtt_publish`, `mqtt_subscribe` and `mqtt_unsubscribe`, so a
  static handler can echo it with `{{event.packet_id}}` — no script and no model call:

  ```json
  {"event_pattern": "mqtt_publish",
   "handler": {"type": "static",
     "actions": [{"type": "mqtt_puback", "packet_id": "{{event.packet_id}}"}]}}
  ```

- `mqtt_suback`'s `granted_qos` must have one entry per filter in the event's `topics`,
  in the same order.
- QoS 0 publishes carry no packet identifier; the event reports `packet_id: 0` and no
  acknowledgement exists.

### Payloads are text, never bytes

`payload` is the message body decoded as UTF-8. If the bytes are not valid UTF-8,
`payload_is_text` is false, `payload` holds a lossy rendering and `payload_size` gives
the true byte count. Nothing base64 or hex encoded is ever put in an event or an action —
a model cannot reliably read or write it.

## Actions

**Sync** (in response to a client packet):

| Action | Parameters |
|---|---|
| `mqtt_connack` | `return_code` (0 accept; 1-5 refuse), `session_present` |
| `mqtt_suback` | `packet_id`, `granted_qos` (0/1/2 to grant, 128 to refuse, one per filter) |
| `mqtt_puback` | `packet_id` |
| `mqtt_pubrec` | `packet_id` |
| `mqtt_unsuback` | `packet_id` |
| `mqtt_publish` | `topic`, `payload`, `qos`, `retain`, `packet_id`, `to_client_id` |
| `close_this_connection` | — |

**Async** (user-triggered, no connection context):

| Action | Parameters |
|---|---|
| `mqtt_publish_to_client` | `server_id`, `client_id` (or `"*"`), `topic`, `payload`, `qos`, `retain`, `packet_id` |
| `list_mqtt_clients` | `server_id` |

Every declared parameter is read by `execute_action`; there are no dead actions and no
undeclared executor branches. Sync actions write to the connection they were produced
for, through that connection's single writer channel, so a packet can never interleave
with one the read loop is emitting. `mqtt_publish` with `to_client_id` writes to another
client's channel instead; `"*"` fans out to every client on the server.

## Routing: the model does it, not the broker

**There is no subscription table and no retained-message store in Rust.** After a client
publishes, nothing is delivered automatically. The model remembers who subscribed to what
(server memory, or a script's own state) and issues `mqtt_publish` with `to_client_id` for
each recipient. `connected_clients` in the publish event and the `list_mqtt_clients`
action give it the live client ids.

The only cross-request state the broker keeps is a directory of **live socket senders**
keyed by `(server id, client id)`, so that a named recipient can be written to. Nothing
survives a disconnect; no message, subscription, topic or retained value is stored.

## Dashboard peer injection

Every connection registers a **peer handle** (`peer_support::register_peer_channel` /
`spawn_peer_command_task`) right after it is accepted — before any CONNECT — so the
dashboard's "message this peer" / "disconnect this peer" rows work while the connection is
idle. Injected actions run through the same executor the model path uses, against the
per-connection `MqttProtocol`, so an injected `mqtt_publish` / `mqtt_puback` / … is encoded
by exactly the code the model's would be.

Two things follow from MQTT's channel-based writer (`out_tx`, drained by one writer task
that owns the shared `Arc<Mutex<WriteHalf>>`):

- **Wire verbs return `ActionResult::Custom`, not `Output`.** They write to the connection's
  own channel as a side effect and return Custom, so the generic peer task reports the
  outcome as `Executed { detail }` rather than `Sent { bytes_sent }` — but the bytes *do*
  reach the wire, and the byte counters move. This is why the peer-inject test reads the
  socket and the counters, not the outcome's byte count.
- **`close_connection` has an explicit `execute_action` arm** (returning
  `ActionResult::CloseConnection`) because "disconnect this peer" injects
  `{"type":"close_connection"}`, distinct from the model-facing `close_this_connection`
  verb. On it the generic task `shutdown()`s the shared write half → the client reads EOF.

The peer handle is removed on every exit path via `finish_connection`. It is removed
*first* there, because the peer command task holds an `Arc<MqttProtocol>` carrying an
`out_tx` clone; dropping the handle ends that task so the writer can finish draining.

**Per-connection byte and packet counters** are updated on every read (in the read loop)
and every write (in the writer task), so the rail shows live `↓/↑` counts.

## What the broker answers by itself

Two cases, both pure transport bookkeeping with no semantics to decide:

- **PINGREQ → PINGRESP** (keep-alive).
- **PUBREL → PUBCOMP**, the second half of the QoS 2 handshake, echoing the packet
  identifier.

## Failure behaviour

A protocol that stays silent leaves the client blocked until its own timeout, so every
event that owes a mandatory reply has a default:

| Event | If the handler produces no reply | Rationale |
|---|---|---|
| `mqtt_connect` | CONNACK, return code 0 (accept), logged at WARN | CONNACK is mandatory (3.2); refusing on an LLM outage would make the server useless as a honeypot |
| `mqtt_publish` QoS 1 / 2 | PUBACK / PUBREC echoing `packet_id` | otherwise the client republishes forever |
| `mqtt_subscribe` | SUBACK granting the QoS each filter asked for, logged at WARN | SUBACK is mandatory (3.8.4) |
| `mqtt_unsubscribe` | UNSUBACK echoing `packet_id` | mandatory (3.10.4) |

"Produces no reply" is checked per **packet type**, not "wrote anything": a handler that
forwards a PUBLISH but forgets the PUBACK still gets the PUBACK default.

Errors that close the connection: a malformed CONNECT, a second CONNECT on one connection
(3.1.0-2), QoS 3, a SUBSCRIBE with no filter, a malformed packet, and any packet larger
than `max_packet_size`.

## Limits

The fixed header allows a 268 435 455 byte remaining length; trusting it would let one
client request a 256 MiB allocation per packet. `max_packet_size` (startup parameter,
default 256 KiB, hard maximum 16 MiB) is checked before any allocation, and the
connection is closed when it is exceeded. Every parse function is bounds-checked with
`get()`/`checked_add`; no input can panic the read loop.

## Startup parameters

- `max_packet_size` (integer, optional, default 262144, clamped to 64..=16777216)

## Not implemented

- **MQTT 5.0** — `protocol_level` is reported so a handler can refuse a v5 client with
  CONNACK return code 1.
- **TLS (8883) and WebSocket transport.**
- **Retained messages** — the RETAIN flag is reported inbound and settable outbound, but
  nothing is stored, so a late subscriber receives nothing automatically.
- **Last will and testament** — `will_topic` and `will_message` are surfaced, but the
  broker never publishes the will on an unclean disconnect.
- **Session persistence** (`clean_session=false`), **QoS 2 duplicate suppression**,
  **keep-alive enforcement** (an idle client is not disconnected), **topic wildcard
  matching** in Rust (the model interprets `+` and `#` itself), **`$SYS` topics**,
  **clustering**, **bridging**, **authentication beyond passing the username to the
  model**.

## Testing

`tests/server/mqtt/e2e_test.rs` (declared in `tests/server/mod.rs`). Its
`test_mqtt_keyword_detection` still asserts that starting an MQTT broker fails, and its
docstring still describes a placeholder — both predate this implementation and are
outside this change.

Real-client verification used during review, via `--mcp-stdio` with static handlers so no
model is involved: a raw-socket MQTT 3.1.1 client sending CONNECT, SUBSCRIBE, PUBLISH at
QoS 0/1/2 and PINGREQ, asserting CONNACK, SUBACK with the requested QoS, PUBACK/PUBREC
echoing the packet identifier, and PINGRESP.

## References

- [MQTT 3.1.1 (OASIS)](https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html)
- [rumqttc](https://docs.rs/rumqttc/) — client used by the E2E tests
- Testing notes: `tests/server/mqtt/CLAUDE.md`
