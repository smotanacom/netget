# Modbus/TCP client

NetGet is the **master**: it dials a Modbus/TCP device and the model decides which coils,
discrete inputs and registers to read and what to write. TCP only — no RTU, no ASCII.

## Framing: the server's codec, not a second copy and not tokio-modbus

`src/server/modbus/codec.rs` is shared. The client uses `encode_request` / `parse_response`
(added beside the server's `parse_request` / encoders for this client) plus the existing
`try_parse_adu` / `encode_adu`. Two consequences:

- `encode_request` runs its own output back through `parse_request`, the decoder the server
  answers real masters with, so the client never writes a request the specification refuses
  (quantity 0, 126 registers, a range past `0xFFFF`) — the model gets the reason instead.
- `tokio-modbus` stays a dev-dependency used only as the independent client in the server's
  tests. Pulling it into the client would make it the implementation both halves of any
  NetGet-on-NetGet exchange were built on.

The peer that makes this client's evidence count is therefore **not** anything in this crate:
it is pymodbus, a Python implementation, with `mbpoll` (libmodbus, C) reading back.

## Tasks

`mod.rs` runs three tasks per connection, all registered with `spawn_client_task`:

| task | owns | never waits on |
|---|---|---|
| transport | the socket, the request queue, the one transaction on the wire, its deadline | the model |
| turns | the model calls, one queued event at a time | the socket |
| commands | injected actions (`[ send ]`, MCP `send_to_client`) | the model |

**One transaction on the wire at a time.** The implementation guide permits pipelining, but a
device may accept only one outstanding transaction and **pymodbus 3.15 silently drops the second
of two ADUs that arrive in one TCP segment** — found by this client's real-server test, where a
model turn that returned two reads lost the second one with no error at either end. Requests now
queue in the transport; the next is written when the previous one is answered or times out. An
accepted request is reported to an injected-command caller as `Sent { bytes_sent }` when it went
straight onto the wire and as `Executed { detail: "queued behind N request(s)…" }` when it waits.

The chain request → response → model → request passes through the turn queue, so it needs no
recursion and no `MAX_FOLLOWUP_DEPTH`.

## Events

| event | when | data |
|---|---|---|
| `modbus_connected` | TCP connected | `remote_addr`, `unit_id` |
| `modbus_read_response` | FC 1-4 answered | `function`, `unit_id`, `address`, `quantity`, `values` (bools or 0-65535) |
| `modbus_write_response` | FC 5/6/15/16 acknowledged, echo matched | `function`, `unit_id`, `address`, `quantity`, `values` written |
| `modbus_exception` | `function | 0x80` | `function`, `unit_id`, `address`, `code`, `name` (e.g. `illegal_data_address`) |
| `modbus_error` | no usable answer | `kind`: `timeout` (5s), `bad_response` (wrong function code, byte count, length or echo), `unit_mismatch`; `message`, `function`, `address` |

Every response is checked against the request it answers before the model sees it
(`parse_response`): the function code or its exception form, the byte count against the quantity
asked for, the PDU length against the byte count, and a write's echo byte for byte. A response
for a transaction that is not in flight (late, after a timeout) is logged
`decision=unmatched_response` and dropped. A framing error (protocol id ≠ 0, MBAP length outside
2..=254) closes the connection — the next frame's start is unknown.

## Actions

`modbus_read_coils` / `_discrete_inputs` / `_holding_registers` / `_input_registers`
`{address, quantity, unit_id?}`, `modbus_write_single_coil {address, value: bool}`,
`modbus_write_single_register {address, value}`, `modbus_write_multiple_coils {address,
values: [bool]}`, `modbus_write_multiple_registers {address, values: [int]}`, `disconnect`.

Numbers are **refused, never narrowed**: `u16::try_from` / `u8::try_from`, so register value
65536 is an error rather than 0 and unit id 257 is an error rather than 1. `unit_id` defaults to
the `unit_id` startup parameter (default 1).

## Bounds

| bound | value | test |
|---|---|---|
| ADU size | 260 octets, via the shared `try_parse_adu` (declared MBAP length checked first) | the server's `bounds_test.rs` covers the codec |
| requests accepted and unanswered | 32 (`MAX_QUEUED`) — the next is refused | `unanswered_test.rs` |
| response wait | 5s (`RESPONSE_TIMEOUT`) → `modbus_error {kind: timeout}` | `unanswered_test.rs` |
| events waiting for the model | 256; past that, dropped with `decision=turn_queue_full` | — |

Modbus has no nesting, so there is no depth to bound.

## Idle connections

The client sends nothing until asked. A device (or NetGet's own Modbus server, whose first-byte
bound is 30s) that reaps an idle master closes the socket; the transport reports
`Disconnected`. There is no reconnect.

## Logging

Every model outcome logs `decision=model_actions` / `model_silent` / `llm_error`; the transport
logs `decision=timeout`, `decision=unmatched_response`, `decision=turn_queue_full`.

## Maturity: Beta

All four conditions of the client bar hold, on `tests/client/modbus/real_server_test.rs`:

1. the peer is pymodbus 3.15 — a Python implementation with its own framer and datastore, not
   the codec this client frames with — and `mbpoll` (libmodbus) reads back;
2. a missing `python3`, `pymodbus` or `mbpoll` fails the test, never skips it;
3. a real session: FC 4, 16, 5, 2 and 3 answered by the device, including its own exception;
4. the client acts on the model's answer, asserted from the device's side by `mbpoll` — and
   emptying the loop over `result.actions` fails it.

`python3 scripts/beta_evidence_table.py --side client` reads the peer as `python3 pymodbus`
(`PYTHON_THIRD_PARTY_PROTOCOL_SERVERS`). Passed three consecutive runs at `--test-threads=100`
before promotion.
