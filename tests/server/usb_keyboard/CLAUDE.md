# USB Keyboard E2E Tests

## What these prove, and what they do not

One question: *the model says "type hi" — do the bytes a host reads off the interrupt endpoint
spell `hi`?*

The tests drive a **real USB/IP client over TCP** (`tests/helpers/usbip_client.rs`):
`OP_REQ_DEVLIST`, `OP_REQ_IMPORT`, then interrupt transfers on endpoint 1 and control transfers
on endpoint 0. At the USB/IP layer an interrupt transfer is indistinguishable from a bulk one —
both are `USBIP_CMD_SUBMIT` carrying an endpoint *number* — so `bulk_in` drives the HID endpoint.
Reports are decoded against the boot-protocol layout (modifier, reserved, six key slots), written
out here rather than reusing netget's encoder.

**This is the device side only.** There is no `vhci-hcd`, no `/dev/input/eventX`, no `evtest` —
macOS has no USB/IP client. A passing run means netget emits the right HID reports for the
model's actions. It does **not** mean Linux turns them into key events.

## What this suite replaced

Six tests that opened a bare `TcpStream` and asserted only that a mock rule fired. They could not
observe a single HID report, so `type_text`, `press_key_combo` and `release_all_keys` were
"tested" with nothing checking what went on the wire — and in fact **every one of those actions
was failing at runtime**, for a reason no test of that shape could see:

> `Action 'type_text' failed: USB keyboard actions require connection_id field in action`

Actions demanded `action["connection_id"].as_u64()`, while every event reported
`connection_id.to_string()` — which is `"conn-2"`, not `2`. A model quoting the event's own field
back always failed, and there was no value it could have sent instead. `connection_id` is now
optional and inferred, and all three forms are accepted.

Two tests were `#[ignore]`d with product-gap notes:

- `usb_keyboard_led_status` — accurate: the crate's `UsbHidKeyboardHandler` discards HID output
  reports, so the event had no emit site, and the crate would in fact have hit its own
  `unimplemented!()` and panicked the session on a `SET_REPORT`. `handler.rs` intercepts it now
  and the test is live, driving a real LED write.
- `usb_keyboard_detached` — stale: the source gained an emit site before this pass.

## Tests

| Test | Proves | LLM calls |
|---|---|---|
| `test_usb_keyboard_types_what_the_model_asked_for` | devlist advertises the HID class and the device imports; `type_text("hi")` produces `h` down / all-zero release / `i` down / release with the right HID usage codes; `press_key_combo(["ctrl","c"])` sets the left-control modifier bit with `c` in the first key slot; **every report is 8 bytes** | 2 |
| `test_usb_keyboard_led_status_reaches_the_model` | a real `SET_REPORT(Output)` with the Caps Lock bit raises `usb_keyboard_led_status` with `caps_lock: true`; `GET_REPORT` returns the LED byte back; an identical repeat raises **no** second event | 3 |
| `test_usb_keyboard_detach` | closing the USB/IP session raises `usb_keyboard_detached` | 3 |

**LLM budget: 8 calls.** The first test bundles both keyboard actions into one attach response.

Two assertions are load-bearing and worth keeping:

- **8 bytes.** The crate answers a key release with `vec![0; 6]`. A boot-protocol report is 8
  bytes — modifier, reserved, six key slots — and a short release is a different message with the
  same intent, which a strict HID parser reads as malformed rather than as "all keys up". The
  wrapper produces 8; the test pins it.
- **No duplicate LED event.** `expect_calls(1)` on the LED rule is what catches a server that
  re-raises on every identical `SET_REPORT`. Hosts re-assert the LED byte routinely (X11 does it
  periodically), and without the change-detection the model would be woken by a stream of
  identical events.

## Synchronisation

Every test waits for `"USB keyboard LLM call completed for connection"` before asserting: the attach event follows the peer's `OP_REQ_IMPORT`, not the TCP accept — a bare
connect costs no model call at all (see `src/server/usb/CLAUDE.md` and
`tests/server/usb_keyboard/attach_on_import_test.rs`), so the log line is the only
signal that the import has been answered. The
LED test additionally waits for `"USB keyboard LLM call completed (led_status)"` — the event kind
is in the line precisely so a test can wait on one specific event with a substring match, rather
than on a call count that a racing event would also satisfy.

`read_reports` polls the IN endpoint the way a host does, with a 10s ceiling so a device that
stops producing fails rather than hangs.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features usb-keyboard \
    --test server -- --test-threads=100 usb_keyboard
```

Under two seconds. Needs `libusb-1.0`; not available in Claude Code for Web. **Run it twice**:
the first run after a source edit relinks the `netget` binary the tests spawn.

## Not covered

- A real Linux host: `sudo usbip attach`, `vhci-hcd`, `evtest`, `xdotool key Caps_Lock`.
- `typing_speed_ms` pacing — the paced path spawns a task and is not exercised.
- `release_all_keys`.
- Characters outside what `key_name_to_hid` maps; the refusal path (which errors rather than
  typing a different string) is not asserted.
- Two hosts attached at once, and therefore the multi-candidate branch of `resolve_handler`.
- N-key rollover, multimedia keys, and anything outside the boot protocol.
