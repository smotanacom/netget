# BLE Presentation Clicker

**Maturity: Experimental.** BLE presentation clicker - HID Service (0x1812) sending page up/down keys

## What this protocol actually is

A thin wrapper over the `bluetooth-ble` base stack. `spawn` reads `device_name`, fetches the
user's instruction from server state, wraps it in a preamble naming the HID characteristics and
the report length, and hands the whole thing to `BluetoothBle::spawn_with_llm_actions`.

The only other profile-specific code is the HID report descriptor, the report length and
`build_presenter_report` in `mod.rs` — the *identity* of the profile, and the single source of
truth the startup examples are generated from. No wire path runs through any of it; the base
owns every byte that leaves the radio.

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

Nothing enforces this — the LLM (or a static handler) builds the services with `add_service`. It
is what the startup examples in `actions.rs` construct and what the instruction preamble asks for.

- **`00001812-0000-1000-8000-00805f9b34fb`** (HID Service)
  - `00002a4a-…` HID Information [read] — `11010002`
  - `00002a4b-…` HID Report Map [read] — `hex::encode(HID_PRESENTER_REPORT_DESCRIPTOR)`
  - `00002a4d-…` HID Report [read, notify] — 8 zero bytes, sized from `HID_PRESENTER_INPUT_REPORT_LEN`
  - `00002a4c-…` HID Control Point [write_without_response]

**The report map and the report length are generated from the constants in `mod.rs`, not
transcribed.** That is structural rather than tidy: nothing in NetGet parses a report
descriptor — the base carries it as an opaque byte string — so a second hand-maintained copy in
the startup examples is a descriptor that drifts silently until a host rejects it. Both
constants and both example values are pinned by
`tests/server/bluetooth_ble_presenter/report_descriptor_test.rs`, which walks every item
against the USB HID 1.11 encoding.

**HID Information is little-endian.** bcdHID 0x0111 is the bytes `11 01`. Written `01 11` —
which this profile and all four of its HID siblings did — a host reads HID version 17.01.

### What the descriptor used to be

Recorded because it is the class of defect, not a one-off. The literal in the startup examples
wrote its padding item as `0x05, 0x75` — `Usage Page (0x75)`, a page the specification reserves
— where `0x75, 0x05` (`Report Size (5)`) was meant: the two bytes were transposed. `Report
Size` therefore stayed at 1 from the button block above it, the pad contributed one bit instead
of five, and the report came to **ten bits** — not a whole number of bytes, and a quarter of the
eight-byte initial value published on the very same characteristic. Nothing failed, because
nothing looked.

**HID-over-GATT caveat:** a host only treats a peripheral as an input device after bonding, and
`ble-peripheral-rust` 0.2 exposes no pairing or bonding control. The layout above is now pinned
against the specification; whether a given OS accepts it as a real input device is a separate
question, needs an adapter and a real central, and is untested here.

## The report layout

`build_presenter_report(control)` in `mod.rs` returns the eight-byte boot-keyboard report the
descriptor declares: modifier byte, the descriptor's constant reserved byte, then six key
slots. A presenter is a keyboard as far as HID is concerned — there is no "next slide" usage
anywhere in the usage tables — so the five controls are ordinary Keyboard/Keypad keycodes:

| Control | Key | Usage |
|---|---|---|
| `next_slide` | PageDown | `0x4E` |
| `previous_slide` | PageUp | `0x4B` |
| `start_presentation` | F5 | `0x3E` |
| `end_presentation` | Escape | `0x29` |
| `blank_screen` | `.` | `0x37` |

It returns `Option`, and that is the point rather than a convenience: **a zeroed HID report is
not "nothing"** — it asserts that every key has been released, which is an unasked-for
statement on the wire. An unrecognised control name therefore yields `None` and the caller
decides. This is the same reasoning that puts the BLE profiles on `CLAUDE.md`'s
deliberately-silent list: a fabricated HID report claims a keypress happened, so on an LLM
failure the correct behaviour is to emit nothing and put the distinction in the log. The base
stack owns every wire write and does that tagging; this profile adds no path that can
synthesise a report, and `build_presenter_report` is called from no wire path at all — it is
the executable statement of the layout the descriptor declares, which the tests hold the
descriptor to.

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

**A `static` handler cannot tell one characteristic from another.** This service has three readable characteristics (HID Information 0x2A4A,
Report Map 0x2A4B and Report 0x2A4D), so its read handler is a **script** in both the
script-mode and static-mode examples, and only the GATT layout is static. A fixed
`respond_to_read` answers *every* readable characteristic with the same bytes, which on a
multi-characteristic service hands the central the wrong value under the right field's units —
invisible until real hardware reads it. A script can see `characteristic_uuid`, costs no LLM
call either, and answering an unrecognised characteristic with `[]` is a real answer:
`read_decision` maps it to `ReadDecision::UseStored`, so the base serves that characteristic's
own `initial_value`.

`tests/server/bluetooth_ble_presenter/gatt_examples_test.rs` pins both rules.

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

- `device_name` (string, optional) — advertised name, default `NetGet-Presenter`

Declared in `get_startup_parameters()`, and read by `spawn` — the only one, in both
directions. That declaration is not optional: an undeclared key is rejected, and the JSON comes
from the LLM or an MCP client. It is **rejected, not fatal** — `StartupParams::new` and every
`get_*` accessor return `Result<_, StartupParamError>`, and parameters are validated *before*
`add_server`, so an undeclared key or a wrong-typed value produces a clean error naming the key
and listing the allowed ones, and leaves no half-registered server behind. Propagate it with
`?`; never `unwrap()`.

They used to panic, which over MCP killed the per-request task before it could reply. This file
asserted that long after it was fixed, which is the wrong direction for a doc to rot in: it tells
the next person to write defensive code around a hazard that is not there.

## Testing

`tests/server/bluetooth_ble_presenter/` exists, is declared in `tests/server/mod.rs`, and its
tests run and pass. This file previously said no test directory existed and that none was
declared — wrong in both halves, and the dangerous direction: it tells the next person a
capability is missing, so they build a second copy around an absence that is not there.

- **`report_descriptor_test.rs`** is where the coverage is: pure unit tests, no adapter, no
  `#[ignore]`, walking the report descriptor item by item against the USB HID 1.11 encoding and
  summing the declared bits. It includes the descriptor that *used* to ship, walked and shown
  to be rejected, so the suite is demonstrated to catch the original defect rather than merely
  agreeing with the fix.
- **`gatt_examples_test.rs`** is the other adapter-free half: every service and characteristic
  UUID and every literal value byte in `get_startup_examples()` is checked against the
  Bluetooth SIG layout, byte order included.
- **`e2e_test.rs`** covers the *wiring* only — that `open_server` with `base_stack:
  "BLUETOOTH_BLE_PRESENTER"` reaches this protocol's `spawn`, that the base brings the radio up,
  and that a `bluetooth_ble_started` event is raised and answered by the mocked model. Nothing
  in it builds the HID Service (0x1812), puts a byte on the wire, or reads one back. It claims
  the machine's Bluetooth adapter, so it needs one present and powered — it is not adapter-free,
  it simply is not `#[ignore]`d.

Raising the rating above `Experimental` needs a real adapter, an independent central and a host
that accepts the peripheral as an input device — and HID-over-GATT needs bonding, which
`ble-peripheral-rust` 0.2 exposes no control over, so it is blocked on the dependency and not
only on hardware. See `tests/server/bluetooth_ble_presenter/CLAUDE.md`.
