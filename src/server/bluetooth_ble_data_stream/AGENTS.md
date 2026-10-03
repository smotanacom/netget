# BLE Data Stream

**Maturity: Experimental.** BLE data streaming - custom GATT service pushing real-time sensor/telemetry data

## What this protocol actually is

A thin wrapper over the `bluetooth-ble` base stack. `spawn` reads `device_name`, fetches the
user's instruction from server state, appends a profile sentence to it, and hands the whole
thing to `BluetoothBle::spawn_with_llm_actions`. There is no other profile-specific code.

That matters more than it sounds, because the base **hardcodes `BluetoothBleProtocol`** when it
calls `call_llm` (`src/server/bluetooth_ble/mod.rs`, the `protocol` local passed to
`call_llm`/`call_llm_for_event`). Consequences:

- The only events ever emitted for this server are the base's five.
- The only actions the model is ever offered are the ones those event types carry via
  `.with_actions(...)` — `call_llm` builds its tool list from `event.event_type.actions`, not
  from `get_sync_actions()`.
- The only actions that ever execute are the ones `BluetoothBle::execute_action` matches.

So this protocol declares **no actions and no events of its own**, and delegates
`get_async_actions`, `get_sync_actions`, `get_event_types` and `execute_action` to
`BluetoothBleProtocol` — the same shape `doh` and `dot` use to forward `DnsProtocol`'s set.
Anything else would be a documented vocabulary that no code path can reach: the model would be
told about `set_x`, return it, and have it rejected as an unknown action, while an
`event_handlers` entry keyed on a profile-specific event id would validate at startup and then
never fire.

**Do not add profile-specific actions or events here.** They belong in the base stack's
executor, or nowhere.

## Actions (delegated from `bluetooth-ble`)

| Action | Kind | Purpose |
|---|---|---|
| `add_service` | async | Add a GATT service and its characteristics |
| `start_advertising` | async | Become discoverable |
| `stop_advertising` | async | Stand down |
| `send_notification` | async | Push a new characteristic value to subscribers |
| `respond_to_read` | sync | Answer `bluetooth_read_request` |
| `respond_to_write` | sync | Acknowledge `bluetooth_write_request` |

## Events (delegated from `bluetooth-ble`)

| Event | Offered actions |
|---|---|
| `bluetooth_ble_started` | `add_service`, `start_advertising` |
| `bluetooth_state_changed` | `start_advertising`, `stop_advertising` |
| `bluetooth_read_request` | `respond_to_read` |
| `bluetooth_write_request` | `respond_to_write`, `send_notification` |
| `bluetooth_subscribe` | `send_notification` |

Script and static handlers registered against these ids are dispatched by
`try_execute_event_handler` inside `call_llm`, so a handled event costs no model call.

## GATT layout this profile suggests

Nothing enforces this — the LLM (or a static handler) builds the services with `add_service`. It is what the startup examples in `actions.rs` construct and what the instruction preamble asks for.

- **`0000f00d-0000-1000-8000-00805f9b34fb`**
  - `0000f00e-0000-1000-8000-00805f9b34fb` [notify]
  - `0000f00f-0000-1000-8000-00805f9b34fb` [read, write] — initial value `00`

## UUIDs: the 16-bit shorthand works, and the examples use the full form anyway

The base parses every service and characteristic UUID with `parse_ble_uuid`
(`src/server/bluetooth_ble/mod.rs`), which **does** accept the 16-bit Bluetooth SIG shorthand: a
4- or 8-character hex string is left-padded to 32 bits and spliced into the BLE base UUID, so
`"180D"` becomes `0000180d-0000-1000-8000-00805f9b34fb`. Anything else is handed to
`Uuid::parse_str`. All four call sites go through it — `add_service` for the service and for
each characteristic, `start_advertising` for the advertised service list, and
`send_notification`.

This section used to say the exact opposite: that the shorthand is rejected, that `add_service`
fails with "Invalid service UUID", that no expansion helper exists anywhere in the tree, and
that `src/server/bluetooth_ble/CLAUDE.md` was wrong to claim `"180D"` is "expanded to" the full
form. Every part of that was false and the base's own documentation was right. Read
`parse_ble_uuid` before repeating any of it.

The startup examples here are still written out in full, deliberately: a 128-bit literal can be
checked against the SIG assigned-numbers list without expanding it in your head, and it is the
form nRF Connect and a `tshark` display filter show. Alias `XXXX` expands to
`0000XXXX-0000-1000-8000-00805f9b34fb`.

## Data format: hex, and why that is not a rule violation

The project rule is that actions must not carry raw bytes or base64, because models cannot
reliably produce or parse them. GATT is the honest exception: a characteristic value *is* an
opaque byte string defined by a Bluetooth SIG spec, and there is no structured field set that
could replace it without inventing a per-characteristic schema for all of the assigned numbers.

The base therefore uses lowercase hex strings for `initial_value`, `send_notification.value`,
`respond_to_read.value` and the inbound `bluetooth_write_request.value`, and it really does
decode them (`hex::decode`, with an optional `0x` prefix stripped) — the documented encoding and
the executor agree, in both directions.

Hex is a deliberate choice over base64: models handle short hex well, and it maps one-to-one
onto the byte layouts printed in the SIG specifications.

## Reading a characteristic: what the startup examples do, and why

Two things about the `bluetooth_read_request` examples are easy to get wrong, and both were
wrong here.

**A Python script handler must read stdin and print its answer.** `python3 -c <code>` is run
unwrapped (`src/scripting/executor.rs`), and the executor requires stdout to be exactly one JSON
value. Every script example in the BLE profiles used to be `actions = [{...}]`, which assigns a
local, prints nothing and exits 0 — so the handler was recorded as having failed and the event
**fell back to the LLM**, the opposite of what the comment above it promised. The shape that
works is:

```python
import json,sys
e=json.load(sys.stdin)['event']
v={'<characteristic-uuid>':'<hex>'}.get(str(e.get('characteristic_uuid','')).lower())
print(json.dumps({'actions':[{'type':'respond_to_read','value':v}] if v else []}))
```

**A `static` handler cannot tell one characteristic from another.** A fixed
`respond_to_read` answers *every* readable characteristic with the same bytes, which on a
multi-characteristic service hands the central the wrong value under the right field's units —
invisible until real hardware reads it. A script can see `characteristic_uuid`, costs no LLM
call either, and answering an unrecognised characteristic with `[]` is a real answer:
`read_decision` maps it to `ReadDecision::UseStored`, so the base serves that characteristic's
own `initial_value`.

`tests/server/bluetooth_ble_data_stream/gatt_examples_test.rs` pins both rules.

## No storage

This protocol stores nothing. The base keeps the last written/notified value per characteristic
purely so a read with no `respond_to_read` in the LLM's reply can fall back to the current
value; that is transport state, not a database. All profile data comes from the instruction,
the LLM, or a script/static handler.

## Privilege and platform

BLE peripheral mode needs **Bluetooth adapter access, not a port**. `PrivilegeRequirement` has
no variant for that (`None` / `PrivilegedPort` / `RawSockets` / `Root`), so the declaration is
left at the default `None`: claiming `Root` would be false — Bluetooth needs no root on macOS or
Windows — and would make `server_startup.rs` refuse to start for unprivileged users who can in
fact use the adapter. See the out-of-scope note in the review notes: the metadata enum needs an
`AdapterAccess`-style variant before this can be declared honestly.

Platform requirements come from `ble-peripheral-rust` 0.2: BlueZ + `bluetoothd` + D-Bus on
Linux (feature needs `libdbus-1-dev`), CoreBluetooth on macOS, WinRT on Windows 10+.

## Startup failure behaviour

Failures propagate correctly — this protocol does **not** have the ARP/DataLink/ICMP defect of
reporting `Running` while doing nothing. `spawn` is awaited by `server_startup.rs`, which turns
an `Err` into `ServerStatus::Error`. Two failure paths exist, both in the base:

1. `Peripheral::new()` fails → `Error("Failed to create BLE peripheral: …")`.
2. The adapter never reports powered → the base polls `is_powered()` 20 times at 500 ms and
   bails → `Error("Bluetooth adapter failed to power on after 10 seconds")`.

Verified on macOS: `Peripheral::new()` succeeds and `is_powered()` returns `false` on the first
poll and `true` on the second, so the retry loop is load-bearing rather than decorative. Path 2
is what a user with Bluetooth switched off, no adapter, or a denied CoreBluetooth permission
gets. The refusal is clear, but the message attributes all three causes to "not powered on".

## Startup parameters

- `device_name` (string, optional) — advertised name, default `NetGet-Stream`

Declared in `get_startup_parameters()`. That is not optional: an undeclared key is rejected, and
the JSON comes from the LLM or an MCP client. It is **rejected, not fatal** — `StartupParams::new`
and every `get_*` accessor return `Result<_, StartupParamError>`, and parameters are validated
before `add_server`, so an undeclared key or a wrong-typed value produces a clean error naming
the key and listing the allowed ones, and leaves no half-registered server behind. Propagate it
with `?`; never `unwrap()`.

They used to panic, which over MCP killed the per-request task before it could reply. This file
asserted that long after it was fixed, which is the wrong direction for a doc to rot in: it tells
the next person to write defensive code around a hazard that is not there.

## Testing

`tests/server/bluetooth_ble_data_stream/` exists, is declared in `tests/server/mod.rs`, and its
tests run and pass. This file previously said no test directory existed and that none was
declared — wrong in both halves. That is the dangerous direction for a doc to rot in: it tells
the next person a capability is missing, so they build a second copy around an absence that is
not there. Re-derive before trusting any "there is no test" claim here.

**What `e2e_test.rs` proves, exactly.** That `open_server` with `base_stack:
"BLUETOOTH_BLE_DATA_STREAM"` reaches this protocol's `spawn`, that the base brings the radio up, and that a
`bluetooth_ble_started` event is raised and answered by the mocked model. That is real coverage
of the *wiring* and **no coverage of the profile**: nothing in it builds the custom streaming service, puts a byte
on the wire, or reads one back. It also claims the machine's Bluetooth adapter, so it needs one
present and powered — it is not adapter-free, it simply is not `#[ignore]`d.

**What `gatt_examples_test.rs` proves.** Not value bytes: this profile's startup examples use
**custom** UUIDs with no Bluetooth SIG layout behind them, so there is no independent
specification to check them against and a test restating them would assert only that the
literals equal themselves. What it checks instead is the example's internal coherence — that
every event it routes can actually be raised by the service it builds, and that a static
`respond_to_read` is not answering several readable characteristics with one set of bytes. That
caught a real defect: the file-transfer example routed `bluetooth_read_request` while both its
characteristics were write/notify, so the handler validated at startup and then never matched,
sitting in the example a model copies as though it worked. The profiles with SIG-assigned
characteristics (battery, heart_rate, thermometer, environmental, weight_scale, cycling,
running, presenter) additionally pin every value byte against the published layout.

**Neither test is evidence for a rating above `Experimental`.** Meaningful coverage of the
profile needs a real adapter and a real BLE central (nRF Connect, `btleplug`) completing a read
or a subscription against a service this profile actually built — no test in the tree does that.
