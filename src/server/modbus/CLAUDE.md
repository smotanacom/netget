# Modbus TCP Protocol Implementation

## Overview

Modbus TCP server. The LLM plays the device: it decides what a coil or register **reads as**,
whether a write is accepted, and which exception to raise when it is not. NetGet owns the wire
format, so the model never sees a transaction id, a byte count or a bit-packing rule.

**Status**: Beta — `metadata()` says so, and the evidence is
`test_modbus_reads_writes_and_exceptions_against_tokio_modbus`, which drives the real
`tokio-modbus` 0.17 client, is not `#[ignore]`d, and has no skip-when-missing gate.
`tokio-modbus` is an **unconditional** dev-dependency, so the evidence compiles wherever the
suite does. (This line said "Experimental" while the code said `Beta` — the code was right.)
Note that the blocking CI `test` job runs `tcp,http,dns,udp,redis,mcp-stdio` only, so it does
not execute this test; run it yourself.
**Spec**: MODBUS Application Protocol Specification V1.1b3; MODBUS Messaging on TCP/IP
Implementation Guide V1.0b
**Port**: 502, declared as `PrivilegeRequirement::PrivilegedPort(502)`
**Feature**: `modbus` (no optional dependencies — the codec is hand-rolled)

### Why `PrivilegedPort(502)` and not `None`

502 is below 1024, so this requirement **genuinely fires** — unlike `svn`'s
`PrivilegedPort(3690)`, which was dead code. It is also not over-claiming: `server_startup.rs`
only enforces a `PrivilegedPort` requirement when the port *actually requested* is below 1024,
so an unprivileged user starting the server on 5020 (as every test and every example here does)
is unaffected.

## Library choice

**Hand-rolled**, in `codec.rs` (~370 lines). `tokio-modbus` has a `tcp-server` feature and was
considered, but:

- Its server API wants a `Service` returning a `Response` enum, which would put the crate's own
  encoder between the model and the wire. Using it on both sides of the tests would then mean
  testing `tokio-modbus` against itself — no evidence at all.
- The format is genuinely small: a 7-byte MBAP header and eight function codes.

`tokio-modbus` 0.17 (MIT OR Apache-2.0) is instead a **dev-dependency**, used as the independent
client in `tests/server/modbus/e2e_test.rs`. That asymmetry is the point.

## What is implemented

| FC | Name | Request → response |
|---|---|---|
| 0x01 | Read Coils | start+quantity → byte count + LSB-first packed bits |
| 0x02 | Read Discrete Inputs | same |
| 0x03 | Read Holding Registers | start+quantity → byte count + big-endian u16s |
| 0x04 | Read Input Registers | same |
| 0x05 | Write Single Coil | address+0x0000/0xFF00 → request echoed |
| 0x06 | Write Single Register | address+value → request echoed |
| 0x0F | Write Multiple Coils | start+quantity+bytes → start+quantity |
| 0x10 | Write Multiple Registers | start+quantity+bytes → start+quantity |

Exception responses are `function_code | 0x80` followed by the exception code: 0x01 illegal
function, 0x02 illegal data address, 0x03 illegal data value, 0x04 server device failure,
0x0B gateway target device failed to respond.

**Not implemented**: Modbus RTU and ASCII (this is TCP only), FC 0x07/0x08/0x0B/0x0C/0x11/0x14/
0x15/0x16/0x17/0x18/0x2B, and Modbus Security (TLS).

## Architecture decisions

### 1. No register file — the model answers every read

There is deliberately **no storage** in this implementation: no register array, no coil bitmap,
no persistence. Every read raises an event and the model supplies the values. This is the
project rule from `CLAUDE.md`, and it is also the whole point of the protocol here — an
LLM-invented tank level that drifts over successive reads is more interesting than a static
array, and it is something only a model can produce.

Continuity across requests comes from **server memory** (`set_memory` / `append_memory`, both
common actions available on every event) or, if real persistence is wanted, from the generic
SQLite facility. A protocol-local database would be the wrong answer.

### 2. Spec-determined failures never reach the model

`codec::parse_request` returns `Err(exception_code)` for everything the specification decides on
its own: an unknown function code is *always* 0x01, a quantity of 0 or 2001 coils is *always*
0x03, a range running past 0xFFFF is *always* 0x02. Those are answered directly from
`mod.rs`, with no LLM call.

The model is only asked the questions a device actually answers: what does this read return, and
do I accept this write.

### 3. Framing is server-side, so the actions carry meaning rather than bytes

Actions return `ActionResult::Custom` with structured data — `{"values": [1834, 1450]}`,
`{"values": [true, false]}`, `{"exception_code": 2}` — and `mod.rs` builds the PDU and the MBAP
header from the request it parsed. Consequences:

- The model cannot desynchronise a transaction id or emit a wrong byte count.
- The model never has to echo an identifier, which is exactly the failure mode that makes
  static handlers useless for DNS.
- There is no raw-bytes or base64 action parameter anywhere in this protocol, and therefore no
  encoding to get wrong. (`send_tcp_data`'s documented-but-not-decoded `hex` field is the
  cautionary case; it cannot recur here because nothing here takes encoded bytes.)

### 4. Fail closed

`pdu_from_results` answers with exception **0x04 server device failure** and an ERROR log when:

- the LLM call itself failed;
- no usable action came back;
- the model returned the wrong *kind* of answer (bits for a register read, a write-ack for a
  read);
- the model returned the wrong *number* of values for the requested quantity;
- a register value was outside 0-65535, which drops it and so makes the count wrong.

None of those produces a plausible-looking response. A truncated register block would be worse
than an exception: the client would believe it.

**Silence is the wrong answer here, and that is not the general rule.** ~20 protocols in this
project are deliberately silent on LLM failure because every reply they define is a positive
assertion. Modbus is not one of them: it has an exception response, 0x04 means exactly "an
unrecoverable fault occurred while handling this request", and a client that receives it
retries or alarms instead of blocking until its own timeout. What must never happen is a
*value* — a fabricated register read is an assertion about plant state.

### 4b. `decision=` distinguishes the three ways 0x04 reaches the wire

The wire cannot carry the distinction: a model that deliberately refuses with exception 0x04
and a model that never answered at all both put `04` in the PDU. So every request logs a
stable `decision=` token, as `src/server/radius/` does:

| token | meaning | level |
|---|---|---|
| `model_answer` | the model supplied the values, or accepted the write | DEBUG |
| `model_reject` | the model chose an exception itself | DEBUG |
| `spec_reject` | `parse_request` decided; the model was never asked | DEBUG |
| `unit_mismatch` | addressed to another unit id | WARN |
| `fail_closed_llm_error` | the LLM backend failed | **ERROR** |
| `fail_closed_no_action` | the model was asked and returned nothing usable | **ERROR** |
| `fail_closed_wrong_shape` | wrong kind of answer, or wrong number of values | **ERROR** |

`grep 'decision=fail_closed_'` finds every request the model did not actually answer. The
error text itself never reaches the wire — only the exception code does.

### 4c. Model-supplied numbers are refused, never narrowed

`execute_action` rejects a register value outside 0-65535 and an `exception_code` outside the
set the specification defines (1-6, 8, 10, 11), rather than casting. `65736 as u16` is `200`,
which on this protocol is a plausible-looking tank level with no error anywhere; `0x104 as u8`
is `0x04`, a fail-closed answer manufactured out of a typo. `pdu_from_results` filters rather
than casts for the same reason, so a value that somehow arrived out of range makes the count
wrong and becomes an exception instead of a reading.

Addresses and quantities cannot wrap at all: they are `u16` straight off the wire, never
model-supplied, and `check_range` rejects `start + quantity > 0x10000` in `u32` arithmetic.
`tests/server/modbus/e2e_test.rs::test_out_of_range_model_values_are_refused_not_narrowed`
pins all of this.

### 5. Connection state machine

Copied in shape from `src/server/tcp/mod.rs`: `Idle → Processing → Accumulating`, with one
addition — the per-connection `buffer` doubles as the **framing accumulator** and as the queue
for bytes that arrive while an LLM call is in flight. `Accumulating` means "a partial ADU is
buffered"; `Processing` means "a request is with the model". A second write landing mid-call is
appended and picked up by the loop that is already running, so requests on one connection are
answered in order and never concurrently.

The connection is registered in the map **synchronously in the accept loop**, before the reader
task is spawned, for the same reason TCP does it: a client that writes immediately after
`connect()` must not lose its first frame.

A framing error (`protocol_id != 0`, or an MBAP length outside 2..=254) closes the connection.
Neither is answerable — we no longer know where the next frame starts — and the buffer is capped
at `MAX_BUFFERED` (eight ADUs' worth) so a peer that never sends a parseable frame cannot grow
it without bound.

**That cap is enforced on append as well as in the framing loop, and the loop alone was not
enough.** While a request is with the model, `handle_data` appends the new bytes and returns
early; the loop-side check does not run again until the LLM call returns. At the shipped
`--llm-queue-timeout` of 120s that left the queue unbounded for two minutes per request, which
is long enough for a flooding peer to push gigabytes into it. Both sites now check, and both
close the connection rather than trimming — a peer that overruns the frame accumulator has
desynchronised anyway.

### 6. Dashboard injection (peer handle)

Every connection registers a peer handle (`peer_support::register_peer_channel` +
`spawn_peer_command_task`) right after it is tracked, and removes it on every exit path (EOF,
read error, `close()`), so the rail offers `[ disconnect this peer ]`. Byte and packet counters
are updated on every read and every write.

What is injectable is narrow, and honestly so: the four wire verbs return
`ActionResult::Custom` and are framed against the request being answered (transaction id,
quantity), so an injected `send_modbus_*` is reported as "executed (nothing to write)" — a
Modbus server never initiates, so there is nothing it *could* write unprompted. The one verb
that reaches the wire from outside is `close_connection` (an explicit arm in
`execute_action`, not offered to the model), which half-closes the socket so the peer reads
EOF. No bespoke Custom path was added.

This is deliberately **not** the redis-style refactor (encoders moved into `execute_action`
returning `Output`), and moving them would not help: the MBAP header needs the request's
transaction id and unit id, `encode_bits_response`/`encode_registers_response` validate
against the request's quantity, and `encode_write_ack` literally echoes the request — none of
which exists outside `handle_data`'s request context. More fundamentally, Modbus TCP has no
protocol-legal unsolicited server frame: a client matches responses to requests by
transaction id, so anything injected between exchanges would either be dropped or desync the
master. There is no fix to make; `close_connection` is the entire meaningful surface, and
`tests/server/modbus/peer_inject_test.rs` pins exactly that.

## LLM integration

### Events (all three are emitted)

| Event | Raised for | Actions offered |
|---|---|---|
| `modbus_read_bits` | FC 1, 2 | `send_modbus_bits`, `send_modbus_exception` |
| `modbus_read_registers` | FC 3, 4 | `send_modbus_registers`, `send_modbus_exception` |
| `modbus_write_request` | FC 5, 6, 15, 16 | `send_modbus_write_ack`, `send_modbus_exception` |

Every event data payload carries `unit_id`, `function_code`, `function`, `start_address` and
`quantity`; reads add `bit_type` (`coil` / `discrete_input`) or `register_type`
(`holding` / `input`) so the model knows whether it is being asked about an output it can also
write or a read-only measurement; writes add `coil_values` or `register_values`.

There is no event for an unsupported function code, because there is nothing to decide — see
decision 2.

### Actions

- `send_modbus_bits { values: [bool] }` — answers FC 1/2. Length must equal `quantity`.
- `send_modbus_registers { values: [int 0..65535] }` — answers FC 3/4. Length must equal
  `quantity`.
- `send_modbus_write_ack {}` — accepts a write; the echo is built from the request.
- `send_modbus_exception { exception_code }` — refuses. Accepts a number the specification
  defines (1, 2, 3, 4, 5, 6, 8, 10, 11) or a name (`"illegal_data_address"`, ...). Anything
  else is refused with a message listing the real set, because an undefined code is one no
  client can interpret.

## Startup parameters

| Name | Type | Effect |
|---|---|---|
| `unit_id` | integer 0-255, optional | When set, requests addressed to a different unit id are answered with exception 0x0B, as a Modbus/TCP gateway would. When omitted the server answers on every unit id, which is what most Modbus/TCP devices do. |

That is the only one, and it is read in `actions.rs::spawn` and used in `mod.rs::handle_data` —
neither declared-but-unread nor read-but-undeclared. Out-of-range values produce a clean `Err`
from `spawn()`, never a panic.

## Known limitations

1. **TCP only.** No RTU, no ASCII, no serial.
2. **Data model addressing is raw.** `start_address` is the protocol address, so a client asking
   for "40001" in the classic numbering sends 0. The event parameter says so; the model is not
   asked to guess.
3. **No Observe-style push.** A Modbus server never initiates; `get_async_actions()` is
   deliberately empty.
4. **Per-connection tasks are untracked**, as elsewhere in the project: `stop_server` aborts the
   accept loop and releases the port, but does not cancel connections already in flight.
5. **Concurrency is per-connection serial.** Pipelined requests on one socket are answered in
   order, one LLM call at a time. That is correct but not fast; use script or static handlers for
   throughput.

## Example prompts

```
listen on port 5020 via modbus
You are a water treatment PLC.
Holding register 0 is the tank level in cm, normally 170-190 and drifting slowly.
Holding register 1 is pump speed in RPM, 0 when coil 0 is off.
Coil 0 is the pump run command; accept writes to it and remember the state.
Any address above 9 does not exist: reply illegal_data_address.
```

```
listen on port 5020 via modbus
Impersonate a temperature transmitter. Input registers 0 and 1 hold a 32-bit
big-endian value which is degrees Celsius times 100. Reject every write with
illegal_function - this device is read-only.
```

## References

- [MODBUS Application Protocol Specification V1.1b3](https://modbus.org/docs/Modbus_Application_Protocol_V1_1b3.pdf)
- [MODBUS Messaging on TCP/IP Implementation Guide V1.0b](https://modbus.org/docs/Modbus_Messaging_Implementation_Guide_V1_0b.pdf)
- [tokio-modbus](https://docs.rs/tokio-modbus) — the test client, not a runtime dependency
