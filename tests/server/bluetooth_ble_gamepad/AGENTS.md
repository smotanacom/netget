# BLE HID Gamepad — tests

| File | Needs an adapter? | `#[ignore]`d? | What it actually asserts |
|---|---|---|---|
| `report_descriptor_test.rs` | no | no | The HID report descriptor and the GATT values in `get_startup_examples()`, against literal spec bytes |
| `e2e_test.rs` | yes (a real radio) | no | The server starts and answers `bluetooth_ble_started`. Nothing about HID |

**LLM call budget: 2** — one to interpret the instruction, one for `bluetooth_ble_started`.
Both are mocked; `wait_for_mocks(30)` waits for the exchange and `verify_mocks()` asserts it.

The HID item walker is `tests/server/bluetooth_ble_keyboard/hid_descriptor.rs`, reached with
`#[path]`; its header explains why it exists and why it lives there.

## The defect this caught

The report map in the startup example appended a `Report Count (1)` / `Input (Constant)` pad to
an already byte-aligned sixteen bits. That reads as harmless tidiness and is not: it makes the
report **seventeen bits**, so a host pads it to three bytes while every value the profile
publishes is two, and the button bits land where nothing expects them. `sixteen_buttons_need_no_padding`
asserts there is exactly one `Input` item, and `assert_describes_report_of` would fail the
alignment check on its own.

Sixteen buttons at one bit each is exactly two bytes. There is nothing to pad.

## What else it pins

- The descriptor is byte-for-byte the literal written out in the test, each item's meaning spelled
  beside it, and describes exactly `HID_GAMEPAD_INPUT_REPORT_LEN` (2) bytes.
- All sixteen button usages are named, counted through the walker's expansion of the
  `Usage Minimum (1)` / `Usage Maximum (16)` range.
- The startup examples publish those same bytes — `get_startup_examples()` hex-encodes
  `HID_GAMEPAD_REPORT_DESCRIPTOR` rather than carrying a transcription, and the test asserts the
  equality so the derivation cannot be quietly undone.
- HID Information (0x2A4A) decodes to bcdHID 0x0111 little-endian. It was written `01110002`,
  byte-swapped, which a host reads as HID version 17.01.

## Buttons only, deliberately

This profile declares no axes. The e2e test's prompt mentions "analog sticks" and the descriptor
does not provide them — the descriptor is right and the prompt is decoration, because a startup
example is what a model copies onto a real GATT table and a descriptor promising stick bytes that
nothing ever fills is worse than one describing only what is here. If axes are wanted later, they
belong in `HID_GAMEPAD_REPORT_DESCRIPTOR` with the report length and this test updated together.

## What would earn a rating above `Experimental`

An adapter plus an **independent central** that bonds, reads the report map, subscribes to the
input report, and confirms button 1 arrives as button 1. That test does not exist and cannot be
written to run unattended here: HID-over-GATT needs bonding, `ble-peripheral-rust` 0.2 exposes no
bonding control, and an adapter-claiming test would have to be `#[ignore]`d so a 100-thread run
does not deadlock on the machine's single radio — which per the root `CLAUDE.md` is not evidence
however good the reason.
