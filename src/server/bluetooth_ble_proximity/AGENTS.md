# BLE Proximity / Find Me

**Maturity: Experimental.** BLE Proximity / Find Me - Immediate Alert (0x1802), Link Loss (0x1803), Tx Power (0x1804)

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

- **`00001802-0000-1000-8000-00805f9b34fb`**
  - `00002a06-0000-1000-8000-00805f9b34fb` [write_without_response] — initial value `00`
- **`00001803-0000-1000-8000-00805f9b34fb`**
  - `00002a06-0000-1000-8000-00805f9b34fb` [read, write] — initial value `00`
- **`00001804-0000-1000-8000-00805f9b34fb`**
  - `00002a07-0000-1000-8000-00805f9b34fb` [read] — initial value `00`

## UUIDs must be written in full 128-bit form

The base parses every service and characteristic UUID with `uuid::Uuid::parse_str`, which
accepts the 36-character hyphenated form (and the 32-character simple form) and **rejects the
16-bit Bluetooth SIG shorthand**. `"180D"` does not parse, and `add_service` fails with
"Invalid service UUID". There is no expansion helper anywhere in the tree, despite
`src/server/bluetooth_ble/CLAUDE.md` claiming `"180D"` is "expanded to"
`0000180d-0000-1000-8000-00805f9b34fb`.

Every UUID in this protocol's startup examples is therefore written out in full. Alias `XXXX`
expands to `0000XXXX-0000-1000-8000-00805f9b34fb`.

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

- `device_name` (string, optional) — advertised name, default `NetGet-Proximity`

Declared in `get_startup_parameters()`. That is not optional: an undeclared key is
**rejected**, and the JSON comes from the LLM or an MCP client. `StartupParams::new` and
every `get_*` accessor return `Result<_, StartupParamError>`, and `spawn` propagates with
`?`, so an unknown key produces a clean error naming it and listing the allowed ones and
leaves no half-registered server behind. (They used to panic, which over MCP killed the
per-request task before it could reply; that is fixed, and this file said otherwise until
September 2026.)

## Testing

`tests/server/bluetooth_ble_proximity/` exists and is declared in `tests/server/mod.rs`. It
holds two things:

- **`gatt_layout_test.rs`** — pure unit tests over the GATT layout in `get_startup_examples()`.
  No adapter, no radio, no `#[ignore]`. It asserts the three services carry the characteristics
  the SIG assigns them, that Immediate Alert's Alert Level is write-only while Link Loss's is
  read/write, that Tx Power Level is read-only, that every published value is a single octet in
  the range its specification defines, and that all three services are advertised.
- **`e2e_test.rs`** — starts the server against a mocked model and asserts the
  `bluetooth_ble_started` round trip. That proves startup and the base's LLM plumbing, and
  **nothing about the Proximity profile**.

Neither is evidence for a rating above `Experimental`. Proving Find Me or Path Loss behaviour
needs an adapter and an independent central (nRF Connect, `btleplug`).
