# BLE Proximity / Find Me — tests

| File | Needs an adapter? | `#[ignore]`d? | What it actually asserts |
|---|---|---|---|
| `gatt_layout_test.rs` | no | no | The GATT layout in `get_startup_examples()` against the SIG Proximity and Find Me specifications |
| `e2e_test.rs` | yes (a real radio) | no | The server starts and answers `bluetooth_ble_started`. Nothing about the profile |

**LLM call budget: 2** — one to interpret the instruction, one for `bluetooth_ble_started`.
Both are mocked; `wait_for_mocks(30)` waits for the exchange and `verify_mocks()` asserts it.

## Not a HID profile

Unlike its four siblings in this family this profile carries no report descriptor, so it does not
use the HID item walker. Immediate Alert (0x1802), Link Loss (0x1803) and Tx Power (0x1804) are
three services of one characteristic each, and what can go wrong is the layout rather than a byte
encoding. The base stack builds whatever service the model describes and validates none of it, so
the startup example is the only place the layout is stated and nothing checked it.

The layout was in fact correct — this profile was the soundest of the five audited — and the
tests exist to keep it that way rather than to repair it.

## What `gatt_layout_test.rs` pins

- Three services, each with the characteristic the SIG assigns it: Alert Level (0x2A06) under
  both Immediate Alert and Link Loss, Tx Power Level (0x2A07) under Tx Power. Two different
  services carrying the *same* characteristic is the part of this profile most often got wrong.
- **Immediate Alert's Alert Level is write-without-response and not readable.** The spec makes it
  write-only because it is a command ("start alerting"), not a state; a readable one invites a
  central to poll it as though it meant something.
- Link Loss's Alert Level *is* state and is read/write. Tx Power Level is read-only — a central
  cannot set the radio's output power.
- Every published value is a single octet. Alert Levels are within the three values the
  enumeration defines (0 No Alert, 1 Mild, 2 High); Tx Power Level is a signed dBm within the
  specified -100..=20.
- All three services are advertised, so a central scanning for Find Me actually finds this
  peripheral.

## What would earn a rating above `Experimental`

An adapter plus an **independent central** — nRF Connect by hand, or `btleplug` in the suite —
that discovers the three services, writes 0x02 to Immediate Alert and observes NetGet act on it,
and drops the link to see the Link Loss alert fire. The last is the one that matters and the one
a lenient test would skip: Find Me is a behaviour on disconnect, and nothing about the GATT table
proves it happens.

That test does not exist. An adapter-claiming test would have to be `#[ignore]`d so a 100-thread
run does not deadlock on the machine's single radio, and per the root `CLAUDE.md` an `#[ignore]`d
test is not evidence however good its reason.
