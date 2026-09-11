# BLE HID Mouse — tests

| File | Needs an adapter? | `#[ignore]`d? | What it actually asserts |
|---|---|---|---|
| `report_descriptor_test.rs` | no | no | The HID report descriptor, the button masks, and the GATT values in `get_startup_examples()`, against literal spec bytes |
| `e2e_test.rs` | yes (a real radio) | no | The server starts and answers `bluetooth_ble_started`. Nothing about HID |

**LLM call budget: 2** — one to interpret the instruction, one for `bluetooth_ble_started`.
Both are mocked; `wait_for_mocks(30)` waits for the exchange and `verify_mocks()` asserts it.

The HID item walker is `tests/server/bluetooth_ble_keyboard/hid_descriptor.rs`, reached with
`#[path]`; its header explains why it exists and why it lives there.

## The signed-axis assertion is the one to keep

Of the four BLE HID profiles, the mouse is the only one where a wrong byte **silently reverses a
direction** rather than breaking the descriptor outright. `pointer_axes_are_signed_and_relative`
asserts two things that no parser would complain about:

- `Logical Minimum (-127)` / `Logical Maximum (127)` — the bytes `15 81 25 7F`. Read as unsigned,
  every leftward movement (-1, encoded `0xFF`) becomes a 255-pixel jump to the right.
- The axis `Input` item sets the Relative bit (`0x06`, not `0x02`). Declared Absolute, the
  pointer teleports to a coordinate instead of moving by a delta.

Both are well-formed descriptors either way, so only an explicit assertion catches them.

## What else it pins

- The descriptor is byte-for-byte the literal written out in the test, each item's meaning spelled
  beside it, and describes exactly `HID_MOUSE_INPUT_REPORT_LEN` (4) bytes: three button bits plus
  five padding bits, then signed X, Y and Wheel.
- Three buttons and three axes, counted through the walker's usage bookkeeping.
- `hid_mouse_buttons` maps Left/Right/Middle onto bits 0/1/2, the order the descriptor declares
  them, and all three fit the three-bit field.
- The startup examples publish those same bytes. `get_startup_examples()` hex-encodes the const
  rather than carrying a transcription, and the test asserts the equality so the derivation cannot
  be quietly undone. **The example that used to sit there was not a descriptor at all**: a
  `Logical Maximum` item written with the four-byte form (`0x27`) swallowed the following
  `Report Size` and `Report Count`, after which the walker finds reserved item types and a 30-bit
  report. It also declared the three buttons as `Report Size 3 × Report Count 3` — nine bits for
  three one-bit buttons.
- HID Information (0x2A4A) decodes to bcdHID 0x0111 little-endian. It was written `01110002`,
  byte-swapped, which a host reads as HID version 17.01.

## What would earn a rating above `Experimental`

An adapter plus an **independent central** that bonds, reads the report map, subscribes to the
input report, and confirms a `-1` X delta moves the pointer *left*. Reading the descriptor back
proves it parses, not that the sign survives.

That test does not exist and cannot be written to run unattended here: HID-over-GATT needs
bonding, `ble-peripheral-rust` 0.2 exposes no bonding control, and an adapter-claiming test would
have to be `#[ignore]`d so a 100-thread run does not deadlock on the machine's single radio —
which per the root `CLAUDE.md` is not evidence however good the reason.
