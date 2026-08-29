# AMQP Client Implementation

## Overview

AMQP 0.9.1 client for connecting to RabbitMQ and other AMQP brokers. Uses lapin library with LLM control.

## Library Choices

**lapin v2.6** - Async AMQP client
- **Rationale**: Mature, actively maintained Rust AMQP 0.9.1 client
- **Features**: Full protocol support, async/await, Tokio integration
- **Compatibility**: Works with RabbitMQ, Azure Service Bus, Apache Qpid
- **Alternatives**: amqprs (newer but less mature)

## Architecture

### Connection Management
- **lapin::Connection**: Main connection to AMQP broker
- **lapin::Channel**: Multiplexed channels for operations
- **Properties**: Default connection properties (locale, heartbeat)

### LLM Integration Points

**Connection Events**:
```rust
AMQP_CLIENT_CONNECTED_EVENT
- Fired when connected to broker
- LLM decides initial actions (open channel, declare resources)
```

**Channel Events**:
```rust
AMQP_CLIENT_CHANNEL_OPENED_EVENT
- Fired when channel created
- LLM can declare queues/exchanges, bind queues, start consumers
```

**Message Events**:
```rust
AMQP_CLIENT_MESSAGE_RECEIVED_EVENT
- Fired when message arrives from queue
- LLM processes message content and decides action (ack, nack, etc.)
```

### Available Actions

**Async Actions** (User-triggered):
- `open_channel`: Create new channel for operations
- `declare_queue`: Declare queue with options (durable, exclusive, auto_delete)
- `declare_exchange`: Declare exchange with type (direct, fanout, topic, headers)
- `bind_queue`: Bind queue to exchange with routing key
- `publish_message`: Publish message to exchange
- `start_consumer`: Start consuming from queue

**Sync Actions** (Network event responses):
- `ack_message`: Acknowledge message delivery
- `nack_message`: Negative acknowledge (reject and optionally requeue)

### State Management

**No Storage** - Following project philosophy:
- LLM tracks which queues/exchanges are declared
- LLM remembers bindings and routing rules
- LLM generates message content as needed
- lapin handles connection/channel state internally

### Dual Logging

All operations logged via:
- **Tracing macros**: `info!`, `debug!`, `trace!` → `netget.log`
- **status_tx channel**: → TUI for real-time display

## Limitations

1. **No Local Address**: lapin doesn't expose TCP local address (uses placeholder)
2. **No Transactions**: tx.select/commit/rollback not exposed to LLM
3. **Basic QoS**: Only basic Quality of Service settings
4. **No Publisher Confirms**: Publish confirmations not exposed
5. **Limited TLS Config**: Basic TLS only, no custom certificates via LLM
6. **Single Connection**: One connection per client instance

## Example LLM Interactions

**Connect and Declare**:
```
User: Connect to RabbitMQ at localhost:5672 and declare a queue named "events"
→ Client connects
→ Event: amqp_connected
LLM Action: {
  "type": "open_channel"
}
→ Event: amqp_channel_opened
LLM Action: {
  "type": "declare_queue",
  "queue_name": "events",
  "durable": true
}
```

**Publish Message**:
```
User: Publish "Task complete" to exchange "work" with routing key "completed"
LLM Action: {
  "type": "publish_message",
  "exchange_name": "work",
  "routing_key": "completed",
  "message_body": "Task complete"
}
```

**Consume Messages**:
```
User: Start consuming from queue "tasks"
LLM Action: {
  "type": "start_consumer",
  "queue_name": "tasks"
}
→ Messages arrive
→ Event: amqp_message_received (for each message)
LLM processes and decides to ack:
LLM Action: {
  "type": "ack_message",
  "delivery_tag": 123
}
```

## Testing Approach

See `tests/client/amqp/CLAUDE.md` for testing strategy.

## Future Enhancements

1. **TLS Configuration**: Allow LLM to configure TLS certificates
2. **Publisher Confirms**: Expose confirm mode to LLM
3. **QoS Control**: Allow LLM to set prefetch limits
4. **Transaction Support**: Expose tx methods for atomic operations
5. **Dead Letter Exchanges**: Support DLX configuration
6. **Message TTL**: Support per-message and per-queue TTL

## References

- [lapin Documentation](https://docs.rs/lapin/)
- [RabbitMQ Tutorials](https://www.rabbitmq.com/getstarted.html)
- [AMQP 0.9.1 Spec](https://www.rabbitmq.com/resources/specs/amqp0-9-1.pdf)

---

# Corrections and the command channel (August 2026)

**Most of the "Available Actions" list above is aspirational and always was.** The client's real
vocabulary is what `actions.rs` declares and `execute_action` accepts: `open_channel`, `publish`,
`disconnect`, `wait_for_more`. There is no `declare_queue`, `declare_exchange`, `bind_queue`,
`start_consumer`, `ack_message` or `nack_message` — and no `publish_message`; the publish verb is
called `publish`.

## What the connection loop actually does now

- **The connected-event actions are executed.** They used to be parsed, logged and thrown away
  (`Ok(_result) => info!("AMQP client ready after connect event")`), so `open_channel` on
  `amqp_connected` — the shape this protocol's own static-mode startup example shows — did
  nothing at all.
- **`Connection::run()` is gone.** It is a *blocking* call and was made from inside a tokio task:
  it parked a runtime worker for the lifetime of every AMQP client and never noticed the
  connection closing either. A supervisor task now polls `conn.status().connected()` and the
  client's presence in `AppState`, and runs the disconnect path when either goes.
- **Opened channels are held** in `AmqpSession.channels`. A `lapin::Channel` closes when its
  handle drops, so an `open_channel` that dropped the handle opened and shut the channel in one
  breath.

## Command channel — the dashboard's `[ send ]`

Adopted, archetype **(a)**: the connection lives in an `Arc<AmqpSession>` that both the
connected-event path and the command loop hold. This client had no loop at all before, so the
command loop is new; what it executes is not — every action goes through the protocol's own
`execute_action` and then the shared `apply_action`.

The channel is registered **before** the `amqp_connected` LLM call, which a manual `*` rule parks
until a human answers; `tests/client/amqp/command_channel_test.rs` guards that with
`wait_for_client_handle` before it sends anything.

| Outcome | When |
|---|---|
| `Executed { detail }` | the method completed on the wire: `Channel.Open/Open-Ok completed; channel 1 is open`, or `Basic.Publish of 19 bytes to exchange "" routing key "tasks" on channel 1` |
| `Rejected { error }` | `execute_action` refused it (unknown name, missing `routing_key`/`payload`) |
| `Disconnected` | `disconnect`; `Connection.Close` was sent |
| `Err(..)` | lapin returned an error (broker gone, channel refused) |

**There is deliberately no `Sent { bytes_sent }`.** `Channel.Open` is a real round trip — lapin
resolves it only when `Open-Ok` comes back — and `basic_publish().await` is what puts the method,
content-header and body frames on the socket. But lapin frames and writes them internally and
reports no byte count, so there is no honest number for `bytes_sent`.

`publish` opens a channel on demand if none is open yet (the detail says so), and otherwise uses
the most recently opened one. Publisher confirms are not enabled, so the returned
`PublisherConfirm` is dropped.

## Still missing

Consuming (`basic_consume`), queue/exchange declaration and binding, and acks — so
`amqp_message_received` and `amqp_channel_opened` are declared events that nothing ever emits.

## The model's answer is executed, bounded (August 2026)

`raise_amqp_event` executed nothing, with this reason: *"the connect path and the command loop
own the session, and a delivery-driven action chain would be unbounded on a busy queue."*

The first half is false — the session is an `Arc<AmqpSession>` and `apply_action` already took
it by reference. The second half is true, and the remedy for an unbounded chain is a bound, not
silence: `MAX_FOLLOWUP_DEPTH = 4`, one chain per event, with the limit reported on the status
stream. As written, a model told to consume a queue and republish what it saw was asked on
every delivery and ignored every time.

`apply_action` returns an explicitly boxed `Pin<Box<dyn Future + Send>>`, because
`apply_action` → `raise_amqp_event` → `apply_action` is a cycle of `async fn`s whose opaque
return types cannot be inferred (E0391).

Note the depth is 4 rather than the 6 used elsewhere in the tree: this is the one client where
the event source is a *stream* the broker controls, so the cheaper bound is the safer default.
