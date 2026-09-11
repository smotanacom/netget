# BLE HID Mouse

**Maturity: Experimental.** BLE HID mouse (0x1812) - HID-over-GATT pointing device

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

- **`00001812-0000-1000-8000-00805f9b34fb`**
  - `00002a4a-0000-1000-8000-00805f9b34fb` [read] — initial value `11010002`
    (HID Information: bcdHID `0x0111` = v1.11 as a little-endian uint16, then bCountryCode
    `0x00` and Flags `0x02` NormallyConnectable)
  - `00002a4b-0000-1000-8000-00805f9b34fb` [read] — initial value `05010902a1010901a1000509190129031500250195037501810295017505810105010930093109381581257f750895038106c0c0`
    (the Report Map; `hex::encode(HID_MOUSE_REPORT_DESCRIPTOR)`, not a second copy)
  - `00002a4d-0000-1000-8000-00805f9b34fb` [read, notify] — initial value `00000000`
    (4 bytes: exactly what the report map describes)
  - `00002a4c-0000-1000-8000-00805f9b34fb` [write_without_response]

**These bytes are generated, not transcribed.** `get_startup_examples()` hex-encodes
`HID_MOUSE_REPORT_DESCRIPTOR` and sizes the report from
`HID_MOUSE_INPUT_REPORT_LEN`, so the example a model copies cannot drift from the
descriptor this profile documents. It could before, and had: every one of the four BLE HID
profiles shipped a startup example whose report map a host would have rejected.

**HID-over-GATT caveat:** a host only treats a peripheral as an input device after bonding, and `ble-peripheral-rust` 0.2 exposes no pairing or bonding control. The report map above is well formed — `report_descriptor_test.rs` walks every item of it — but no host has ever been shown it, so whether a given OS accepts this as a real input device is untested. Do not read "the descriptor parses" as "the device works".

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

- `device_name` (string, optional) — advertised name, default `NetGet-Mouse`

Declared in `get_startup_parameters()`. That is not optional: an undeclared key is
**rejected**, and the JSON comes from the LLM or an MCP client. `StartupParams::new` and
every `get_*` accessor return `Result<_, StartupParamError>`, and `spawn` propagates with
`?`, so an unknown key produces a clean error naming it and listing the allowed ones and
leaves no half-registered server behind. (They used to panic, which over MCP killed the
per-request task before it could reply; that is fixed, and this file said otherwise until
September 2026.)

## Testing

`tests/server/bluetooth_ble_mouse/` exists and is declared in `tests/server/mod.rs`. It holds
two things:

- **`report_descriptor_test.rs`** — pure unit tests over the HID report descriptor and the GATT
  values in `get_startup_examples()`. No adapter, no radio, no `#[ignore]`. It decodes every
  item of `HID_MOUSE_REPORT_DESCRIPTOR` against the USB HID 1.11 item encoding, asserts
  the descriptor is byte-aligned and describes exactly `HID_MOUSE_INPUT_REPORT_LEN` bytes,
  and asserts the startup examples publish those same bytes rather than a second transcription.
  This is the only thing in the tree that validates a report map: the base stack carries one as
  an opaque byte string and never parses it, so before these tests the first parser to see it
  was a real host on someone else's machine.
- **`e2e_test.rs`** — starts the server against a mocked model and asserts the
  `bluetooth_ble_started` round trip. That proves startup and the base's LLM plumbing, and
  **nothing about HID**.

Neither is evidence for a rating above `Experimental`. Proving a host accepts this as a real
input device needs an adapter and an independent central (nRF Connect, `btleplug`), and the
bonding that HID-over-GATT requires is not something `ble-peripheral-rust` 0.2 can drive.
