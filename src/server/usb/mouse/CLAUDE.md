# USB Mouse Server Implementation

## Overview

A virtual USB HID mouse exported over USB/IP. The device presents a boot-protocol mouse
interface (class 0x03, subclass 0x01 boot, protocol 0x02 mouse) with a single interrupt IN
endpoint, and the model drives the host's pointer with `move_relative`, `move_absolute`, `click`,
`scroll` and `drag`.

**State: Experimental.** See *What is and is not verified*.

## Layout

| File | What it does |
|---|---|
| `mod.rs` | Accept loop, USB/IP session, the two events, connection state |
| `handler.rs` | `UsbHidMouseHandler` — the `UsbInterfaceHandler`, report descriptor, report queue |
| `actions.rs` | Action/event definitions, handler registry, `execute_action` |

## The protocol did nothing at all, and said so in a comment

Worth remembering, because every outward sign said otherwise — it was registered, rated
`Experimental`, shipped in `all-protocols`, had a complete six-action vocabulary, and a test
suite of seven passing E2E tests.

- `handle_connection` took the accepted socket as **`_stream` and dropped it**. No USB/IP session
  was ever run, so the device could not be enumerated, let alone imported. It logged
  *"NOT YET FUNCTIONAL - waiting for usbip crate mouse support"*, called the LLM once for
  `usb_mouse_attached`, and parked on `sleep(Duration::from_secs(u64::MAX))` forever — leaking
  the task and making `usb_mouse_detached` an event with no emit site.
- Every action in `actions.rs` parsed its parameters, logged
  *"not yet implemented - usbip crate lacks mouse support"*, and returned `NoAction`.
- The premise was stale anyway. `usbip` 0.9 still ships no mouse handler, but netget has had its
  own complete one in `handler.rs` — report descriptor, 4-byte reports, automatic release — for
  some time. Nothing was wired to it. `CLAUDE.md` meanwhile claimed
  "✅ UsbHidMouseHandler from usbip crate", which does not exist in any version.
- The seven E2E tests opened a bare `TcpStream` and asserted only that a mock rule fired, which
  is why none of the above showed up.

The lesson is the one about tests that never speak the protocol: they cannot distinguish a
working device from a socket that gets dropped.

## HID mouse protocol

Report format, 4 bytes:

| Byte | Meaning |
|---|---|
| 0 | Buttons — bit 0 left, bit 1 right, bit 2 middle |
| 1 | X movement, **signed**, -127..127 |
| 2 | Y movement, signed |
| 3 | Wheel, signed; positive is up |

The host polls the interrupt IN endpoint on its own schedule (10ms interval here) and takes one
report per poll, so a queued sequence paces itself on the wire. Nothing in `execute_action`
sleeps — it is a synchronous trait method running on a runtime worker, where a blocking sleep
stalls that worker.

## LLM actions

**move_relative** — `{"type": "move_relative", "x": 300, "y": -5}`. One signed byte per axis means
anything past ±127 becomes several reports; the model is not asked to know that.

**click** — `{"type": "click", "button": "left"}`. Press *and* release. Without the trailing
all-zero report the host sees the button as still held, which turns a click into a stuck drag.

**scroll** — `{"type": "scroll", "direction": "down", "amount": 2}`. One detent per report, capped
at 64: a wheel value of N is not N clicks on every host, and repeated single detents is what a
real wheel produces.

**drag** — `{"type": "drag", "start_x": 100, "start_y": 100, "end_x": 140, "end_y": 120,
"duration_ms": 40}`. Press, then every intermediate movement report **keeps the button held**,
then release. Releasing at the first move is the classic mistake: the host reads a click followed
by a pointer move, and the selection or window drag silently does nothing. `duration_ms` becomes
the step count at the endpoint's 10ms interval, capped at 64 steps.

**move_absolute** — `{"type": "move_absolute", "x": 960, "y": 540, "screen_width": 1920,
"screen_height": 1080}`. A boot-protocol mouse reports *relative* motion and nothing tells the
device where the pointer is, so absolute positioning is done the way every automation tool does
it: slam into the top-left corner, which the host clamps, then move out from a known origin. This
is more reports than a relative move and **visibly moves the pointer to the corner first**. It
also assumes the host applies no pointer acceleration; with acceleration on, the second leg
overshoots.

### Coordinates are bounded, and it is not cosmetic

`split_movement` emits one 4-byte report per 127 units of travel, because a boot-protocol report
carries one signed byte per axis. Nothing bounded the units, so
`{"type": "move_relative", "x": 9223372036854775807}` asked for roughly 7.3e16 reports —
`execute_action` is synchronous and runs on a tokio worker, so it never returned, the worker
never came back, and the queue grew until the process died.

`move_absolute` and `drag` had the same bug in a second shape: both do arithmetic on the
coordinates before anything looks at them (`-screen_width - 127`, `end_x - start_x`,
`dx * step`), each of which overflows `i64` outright. `Cargo.toml` declares no `[profile.dev]`,
so that panics in debug and test builds and wraps in release — the wrong way round.

Travel is now range-checked at full width to +/-65535 per axis and screen dimensions to
1..=65535, both far past any real display, capping one action at 517 reports. `scroll` was
already capped (`.min(64)`) and `drag`'s step count still clamps to 64.

### `connection_id` is optional

Every action takes an optional `connection_id`. With exactly one host attached it is inferred;
with several, omitting it is an error naming the candidates. Three forms are accepted — the
number, the number as a string, and the `conn-N` form the **events themselves carry**. That last
one matters: events report `connection_id.to_string()`, which is `"conn-2"`, not `2`.

## LLM events

- `usb_mouse_attached` — a host imported the device. Carries `connection_id`. Full action set.
- `usb_mouse_detached` — the USB/IP session ended. Declared `with_no_actions()`: there is no wire
  left to write to. This is emitted for the first time; see above for why it never could be.

## When the LLM call fails

A HID mouse owes the host no reply — the interrupt IN endpoint is NAKed whenever the pointer is
not moving, which is most of the time — so the failure path writes nothing to the wire, on
purpose. There is no HID way to say "the device's brain is unreachable": a STALL is a fault a
host answers by resetting or unbinding the device, and `usbip` cannot express one anyway
(an `Err` out of `handle_urb` tears down the whole session). What matters is that a failed call
must never move the pointer or press a button, and it cannot: reports exist only where an
executed action queued them.

Because every outcome looks identical on the wire, the log is where they are separated. Both
LLM call sites tag their outcome:

| Tag | Meaning |
|---|---|
| `decision=fail_closed_llm_error` | the backend erred; dual-logged at ERROR with the full error |
| `decision=no_action` | the model answered, and asked for nothing |
| `decision=model_action` | the model queued one or more reports |

There is no `decision=model_reject`: the protocol advertises no verb a model can refuse with
(`wait_for_more` is deliberately holding still, and counts as an action). The error itself never
leaves the log and the TUI status stream.

`tests/server/usb_mouse/llm_failure_test.rs` pins the silence, the tag, and the fact that the
device stays enumerable through an outage.

## What is and is not verified

**Verified** by `tests/server/usb_mouse/e2e_test.rs`, which speaks USB/IP over TCP and decodes
the HID reports against the boot-protocol layout:

- The device is enumerated (`OP_REQ_DEVLIST`) advertising 03/01/02, and imported.
- `move_relative(300, -5)` produces reports whose signed dx sums to exactly 300 and dy to -5,
  with no buttons held.
- `click` produces a press report with bit 0 set followed by an all-zero release.
- `scroll down 2` produces two reports with wheel = -1.
- `drag` presses without moving, holds the button on every intermediate report, covers the full
  delta, and releases at the end.
- Closing the session raises `usb_mouse_detached`.

**Not verified:**

- Any real host. No `vhci-hcd`, no `usbip attach`, no `/dev/input/eventX`, no `evtest` — macOS
  has no USB/IP client.
- `move_absolute` — its corner-slam strategy is untested even in the mock, and is the action most
  likely to behave differently against a real desktop (pointer acceleration, multi-monitor
  layouts, clamping behaviour).
- Multiple hosts attached at once, and therefore the multi-candidate branch of `resolve_handler`.
- `GET_REPORT` on the control endpoint (implemented, nothing asserts on it).

## Build

```bash
./cargo-isolated.sh build --no-default-features --features usb-mouse
```

Needs `libusb-1.0` (the `usbip` crate links it): `brew install libusb pkg-config` on macOS,
`apt-get install libusb-1.0-0-dev pkg-config` on Debian. Not available in Claude Code for Web.

## Testing

```bash
./cargo-isolated.sh test --no-default-features --features usb-mouse \
    --test server -- --test-threads=100 usb_mouse
```

See `tests/server/usb_mouse/CLAUDE.md`.

## References

- **USB HID 1.11**: https://www.usb.org/sites/default/files/documents/hid1_11.pdf
- **HID Usage Tables**: https://usb.org/sites/default/files/hut1_4.pdf
- **USB/IP protocol**: https://docs.kernel.org/usb/usbip_protocol.html
