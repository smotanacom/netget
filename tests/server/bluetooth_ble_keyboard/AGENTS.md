# BLE HID Keyboard — tests

| File | Needs an adapter? | `#[ignore]`d? | What it actually asserts |
|---|---|---|---|
| `hid_descriptor.rs` | — | — | Not a test. A USB HID 1.11 item walker, shared with the mouse, gamepad and remote suites via `#[path]` |
| `report_descriptor_test.rs` | no | no | The HID report descriptor, `char_to_keycode`, and the GATT values in `get_startup_examples()`, against literal spec bytes |
| `e2e_test.rs` | yes (a real radio) | no | The server starts and answers `bluetooth_ble_started`. Nothing about HID |

**LLM call budget: 2** — one to interpret the instruction, one for `bluetooth_ble_started`.
Both are mocked; `wait_for_mocks(30)` waits for the exchange and `verify_mocks()` asserts it.

## Why `hid_descriptor.rs` lives here

Nothing inside NetGet parses a HID report map. The base BLE stack carries it as an opaque byte
string, hands it to whichever central asks, and never looks at it — so the first parser to see a
malformed descriptor is a real host, on someone else's machine, after it has shipped. That is
not hypothetical: **all four BLE HID profiles were publishing report maps a host would have
rejected**, and the suite was green throughout.

The walker decodes each item from the encoding in USB HID 1.11 §6.2.2 without calling any NetGet
code, so it is an independent reading of the spec. It catches a mis-sized item, a reserved tag,
an unbalanced collection, a `Report Count` that disagrees with the number of usages in scope, and
a report that is not a whole number of bytes.

It sits in this directory rather than `tests/helpers/` because `tests/helpers/mod.rs` is shared
with every protocol in the tree, and the BLE HID profiles are one agent's boundary. The other
three suites reach it with `#[path = "../bluetooth_ble_keyboard/hid_descriptor.rs"]`, which needs
no module declaration anywhere shared and works whether or not the keyboard feature is enabled.

## What `report_descriptor_test.rs` pins

- The descriptor is byte-for-byte the literal written out in the test, each item's meaning
  spelled beside it — so changing the const forces someone to restate what the new bytes mean.
- It is well formed and describes exactly `HID_KEYBOARD_INPUT_REPORT_LEN` (8) bytes: one modifier
  byte, one reserved byte, six key slots.
- The modifier block names all eight modifier usages and the key array spans 0x00-0x65, checked
  through the walker's usage bookkeeping because both are `Usage Minimum`/`Usage Maximum` ranges
  whose width is invisible in the bytes.
- The startup examples publish those same bytes — `get_startup_examples()` hex-encodes the const
  rather than carrying a transcription, and the test asserts the equality so the derivation
  cannot be quietly undone. The example that used to sit there was **truncated mid-item** and
  declared a ten-byte report against an eight-byte initial value.
- HID Information (0x2A4A) decodes to bcdHID 0x0111 as a little-endian uint16. It was written
  `01110002`, i.e. byte-swapped, which a host reads as HID version 17.01.
- `char_to_keycode` never returns a keycode above the descriptor's `Logical Maximum` of 101, and
  never sets a modifier bit it does not document — swept over 4352 characters. Unmapped
  characters return `None`: a fabricated keystroke is an assertion that a key was pressed.

## What would earn a rating above `Experimental`

An adapter plus an **independent central** — nRF Connect by hand, or `btleplug` in the suite —
that bonds, reads the report map, subscribes to the input report, and reports the character
NetGet intended. The last part is the one a lenient test would skip: reading the descriptor back
proves it is *parseable*, not that byte 2 = 0x04 arrives as `a`.

That test does not exist and cannot be written to run unattended here. HID-over-GATT only becomes
an input device after bonding, and `ble-peripheral-rust` 0.2 exposes no pairing or bonding
control at all. Any adapter-claiming test would also have to be `#[ignore]`d so a 100-thread run
does not deadlock on the machine's single radio — and per the root `CLAUDE.md`, an `#[ignore]`d
test is not evidence however good its reason.
