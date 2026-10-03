# BLE Presenter tests

Two files, and they prove very different amounts.

## `report_descriptor_test.rs` — where the real coverage is

Pure unit tests. No adapter, no radio, no mock model, no `#[ignore]`, no LLM budget: they call
`netget::server::bluetooth_ble_presenter`'s constants and `get_startup_examples()` directly and
compare them against literal USB HID 1.11 and Bluetooth SIG bytes.

This file exists because a malformed HID report map is **invisible from inside NetGet**. The
base BLE stack treats a report map as an opaque byte string, hands it to whichever central asks
for it, and never parses it. The first parser to see one is a host's HID stack on someone
else's machine. The presenter shipped a descriptor that declared a ten-bit report against an
eight-byte published value, and every test in the tree stayed green.

The walker is `#[path]`-included from `../bluetooth_ble_keyboard/hid_descriptor.rs` — the same
module the mouse, gamepad and remote suites use. It is an independent reading of the item
encoding in the HID specification, not a call into NetGet's own code, so it can disagree with
us.

| Test | What it pins |
|---|---|
| `descriptor_matches_the_spec_byte_for_byte` | the const, decoded item by item in the source so a reviewer can check it without a hex editor |
| `descriptor_is_well_formed_and_describes_the_published_report_length` | every item parses, collections balance, and the total is exactly `HID_PRESENTER_INPUT_REPORT_LEN` bytes |
| `three_input_items_make_exactly_eight_bytes` | 8 modifier bits + 8 constant bits + six 8-bit key slots = 64 |
| `the_descriptor_that_shipped_is_rejected_by_the_walker` | **the original bytes, walked, and shown to fail.** Without this the suite only agrees with the fix |
| `every_keycode_is_inside_the_declared_range` | each keycode against the `Usage Minimum`/`Maximum` and `Logical Maximum` read back *out of the walked descriptor*, not restated |
| `reports_carry_the_keycode_and_no_stray_modifier` | `build_presenter_report` sets the key slot and nothing else — in particular no undeclared modifier bit |
| `an_unknown_control_returns_none_rather_than_a_zeroed_report` | an unrecognised name says nothing; a zeroed report would assert every key was released |
| `the_startup_example_publishes_the_descriptor_and_a_matching_report` | the example's `2A4B` and `2A4D` values are generated from the consts and cannot drift |
| `hid_information_says_version_one_point_eleven` | `2A4A` is little-endian `1101…`, not `0111…` (which reads as HID v17.01) |
| `the_script_example_answers_with_a_correctly_sized_report` | the python handler answers with a report of the declared length |
| `the_profile_delegates_its_vocabulary_to_the_base` | the profile's event types and sync actions are the base's verbatim |

## `e2e_test.rs` — startup only

One mocked test, two expectations, **two LLM calls**: the `open_server` instruction and the
`bluetooth_ble_started` event, which is answered with an empty action list. It proves the
protocol is registered, starts through the generic `server_startup.rs` path and round-trips its
first event. It proves **nothing about HID** — no descriptor is exchanged and no central exists.

It waits on `wait_for_mocks(30)` rather than a fixed sleep, then `verify_mocks()`.

## What would raise the rating above Experimental

A real Bluetooth LE adapter, an independent central (nRF Connect or `btleplug`), and a host that
actually accepts the peripheral as an input device. HID-over-GATT additionally requires bonding,
and `ble-peripheral-rust` 0.2 exposes no pairing or bonding control at all — so this is blocked
on the dependency, not just on hardware. Nothing here should be `#[ignore]`d to pretend
otherwise: a skipped test is a silent pass, which `CLAUDE.md` rejects as evidence.
