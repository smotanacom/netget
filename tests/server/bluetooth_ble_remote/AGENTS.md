# BLE Media Remote — tests

Two files, and between them they prove considerably less than this document used to claim.

| File | Needs an adapter? | `#[ignore]`d? | What it actually asserts |
|---|---|---|---|
| `report_descriptor_test.rs` | no | no | The HID report descriptor, `build_remote_report`'s bit assignments, and the GATT values in `get_startup_examples()`, against literal spec bytes |
| `e2e_test.rs` | yes (a real radio) | no | The server starts and answers `bluetooth_ble_started`. Nothing about HID |

**LLM call budget: 2** — one to interpret the instruction, one for `bluetooth_ble_started`.
Both are mocked; `wait_for_mocks(30)` waits for the exchange and `verify_mocks()` asserts it.

## `report_descriptor_test.rs` — where the real coverage is

Nothing inside NetGet parses a HID report map. The base BLE stack carries it as an opaque byte
string, hands it to whichever central asks, and never looks at it. So the first parser to see a
malformed descriptor is a real host, on someone else's machine, after it has shipped — and that
is exactly what happened here. Before September 2026 this profile published a report map in
which `Usage (AC Home)` was written `0x09, 0x23, 0x02`: AC Home is `0x0223`, a two-byte usage,
and the one-byte `0x09` form leaves the trailing `0x02` to be parsed as a fresh item with a
reserved Main tag, which then swallows the two bytes after it. A host's parser stops there.

The startup example was worse and separately wrong: nine controls against a `Report Count` of
eight (so Stop was silently dropped), on entirely different bits from the ones
`build_remote_report` sets, describing a one-byte report where the descriptor const describes
two. A model copying that example onto a real GATT table would have produced a device whose
buttons did the wrong things, if the host accepted it at all.

The tests decode every item against the USB HID 1.11 item encoding (`hid_descriptor.rs`, shared
from the keyboard suite via `#[path]`) and assert:

- the descriptor is byte-for-byte the literal written out in the test, with each item's meaning
  spelled beside it — so changing the const forces someone to restate what the new bytes mean;
- it is well formed, every collection closed, and describes exactly
  `HID_REMOTE_INPUT_REPORT_LEN` bytes;
- the twelve usages are on the Consumer page and in the same order `build_remote_report`
  assigns bits, checked control by control;
- the AC Home encoding: the test *breaks* it back to `0x09` and asserts the walker rejects it,
  so the test is shown to catch the original defect rather than merely agreeing with the fix;
- an unrecognised control name yields `None`, not a zeroed report. A zeroed report is not
  "nothing" — it is a valid HID report asserting every control is released, which is a
  statement this profile has no business making on a caller's typo;
- the startup examples publish the same bytes as the const, and the HID Information
  characteristic decodes to v1.11 little-endian.

## What would earn a rating above `Experimental`

An adapter plus an **independent central** — nRF Connect by hand, or `btleplug` in the suite —
that bonds, reads the report map, subscribes to the input report, and reports the control
NetGet intended. All three parts matter, and the last is the one a lenient test would skip:
reading the descriptor back proves it is *parseable*, not that bit 6 means Volume Up to the
host.

That test does not exist, and it cannot be written to run unattended here. HID-over-GATT only
becomes an input device after bonding, and `ble-peripheral-rust` 0.2 exposes no pairing or
bonding control at all. Any adapter-claiming test would also have to be `#[ignore]`d so a
100-thread run does not deadlock on the machine's single radio — and per the root `CLAUDE.md`,
an `#[ignore]`d test is not evidence however good its reason.

## What this file used to say

It described three test cases — play/pause, volume control, a multi-button sequence — driven by
`btleplug` as a BLE central, with an LLM budget of 2-4 calls each and a note that the tests were
marked `#[ignore]`. None of that existed: there was one test, it used a mocked model, it claimed
no adapter beyond the one the server itself powers on, and it was not `#[ignore]`d. It also
listed "Report descriptor matches Consumer Control format" as something the suite validated,
while the descriptor it would have validated was malformed. Recorded here because a test
document that describes coverage which does not exist is worse than no document: it is the
reason nobody went looking.
