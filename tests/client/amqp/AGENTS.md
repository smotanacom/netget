# AMQP Client E2E Testing

## Strategy

A mocked NetGet **broker** and a mocked NetGet **client** (lapin) over a real socket. No
external RabbitMQ, no Docker.

| Test | Asserts | LLM calls |
|---|---|---|
| `test_amqp_client_connect` | broker's `amqp_connection_open` fires once (protocol header, `Connection.Start`/`Start-Ok` and `Tune`/`Tune-Ok` all crossed the wire); client's `amqp_connected` fires once (the broker's `Connection.Open-Ok` came back) | 4 |
| `test_amqp_client_protocol_detection` | AMQP is selected from three phrasings; the startup rule is used exactly once | 3 |

**Total: 7 LLM calls, ~5s.** Run:

```bash
CARGO_TARGET_DIR=/tmp/clients-target cargo test --no-default-features --features amqp \
    --test client -- --test-threads=100 amqp
```

## What these replaced

`test_amqp_client_connect` could not run at all:

1. The broker rule answered `amqp_connection_received` with `send_amqp_frame`. Neither the
   event nor the action exists on the rewritten broker (`src/server/amqp/`), whose handshake
   event is `amqp_connection_open` and whose reply is `amqp_connection_open_ok`. Nothing
   matched, so the broker never let the client in and `lapin::Connection::connect` never
   returned.
2. The client rule answered `amqp_connected` with `wait_for_more`, which is **not an AMQP
   client action** — the client's entire vocabulary is `open_channel` and `disconnect`
   (`src/client/amqp/actions.rs`) — so `tests/helpers/mock_action_names.rs` panicked while the
   test was being configured.
3. Every `expect_calls` was commented out, so `verify_mocks()` asserted nothing, and the only
   real assertion was `output_contains("AMQP") || output_contains("connect")`.

An orphan `e2e_test.rs.bak` sitting beside them has been deleted.

## What the model is offered

`client_llm_action_set` = async ∪ sync ∪ the firing event's actions, so for this client that is
`open_channel`, `publish`, `consume`, `disconnect` and `wait_for_more`. Anything else a mock
returns is rejected by the response validator as an unknown action, which surfaces as an LLM
failure rather than as a clear test error — so check a mock's action names against
`src/client/amqp/actions.rs`, not against a neighbouring suite.

Three claims that stood here and are **false**, kept because each reads as fact:

- *"`call_llm_for_client` offers the model only `get_async_actions` — no common actions, and
  nothing from `get_sync_actions`."* That was the widest client-side defect in the repo and it
  is fixed; the set is the union above.
- *"the client's entire vocabulary is `open_channel` and `disconnect`"* (said twice). It is the
  five actions above.
- *"`src/client/amqp/mod.rs` discards the result of the `amqp_connected` call, so no
  `Channel.Open` actually goes out."* It executes every returned action through
  `execute_action` → `apply_action`, whose `open_channel` arm calls `conn.create_channel()`.
  `command_channel_test.rs` asserts a real `Channel.Open` round trip.

## Not covered

Queue declare and bind from the client side — the client does not implement them. Publish,
consume and channel open **are** implemented and are covered by `command_channel_test.rs`; the
broker side of everything is covered by `tests/server/amqp/e2e_test.rs`, which drives it with a
real lapin client. Also not covered: TLS (5671), SASL beyond PLAIN, publisher confirms, multiple
channels, and any broker other than NetGet's own.

## `command_channel_test.rs` — the dashboard's `[ send ]`

Same shape as `e2e_test.rs` — a NetGet broker and a real lapin client over a real socket — but
built with `ServerForm`/`ClientForm` and **static** handlers instead of a mocked Ollama, so it
costs **0 LLM calls** (the client's LLM URL is `http://127.0.0.1:1` and its `amqp_connected` call
fails, which the client must tolerate).

Broker handlers: `amqp_connection_open` → `amqp_connection_open_ok` (without it lapin's connect
never returns), `*` → no actions (Basic.Publish owes nothing on the wire outside confirm mode).

Asserts: the command handle exists before anything is sent; `open_channel` returns `Executed`
naming `Channel.Open` (a real round trip — lapin waits for `Open-Ok`); `publish` returns
`Executed` naming `Basic.Publish`; the routing key appears in the **broker's** access log, which
is the proof the frames crossed the socket; an unknown action is `Rejected`; `disconnect` returns
`Disconnected` and the handle goes.
