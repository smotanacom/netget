# USB Mouse E2E Tests

## What these prove, and what they do not

One question: *the model says "move right and click" — do the 4-byte reports a host reads off the
interrupt endpoint say that?*

The tests drive a **real USB/IP client over TCP** (`tests/helpers/usbip_client.rs`):
`OP_REQ_DEVLIST`, `OP_REQ_IMPORT`, then interrupt transfers on endpoint 1. At the USB/IP layer an
interrupt transfer is indistinguishable from a bulk one — both are `USBIP_CMD_SUBMIT` carrying an
endpoint *number* — so `bulk_in` drives the HID endpoint. Reports are decoded against the
boot-protocol mouse layout (buttons, dx, dy, wheel), written out here rather than reusing
netget's encoder.

**This is the device side only.** There is no `vhci-hcd`, no `/dev/input/eventX`, no `evtest` —
macOS has no USB/IP client. A passing run means netget emits the right HID reports for the
model's actions. It does **not** mean a pointer moves on a Linux desktop.

## What this suite replaced, and why it matters

Seven tests that opened a bare `TcpStream` to the USB/IP port and asserted only that a mock rule
fired. They passed while the protocol did **nothing whatsoever**:

- `handle_connection` took the accepted socket as `_stream` and dropped it. No USB/IP session was
  ever run, so the device could not even be enumerated. It logged "NOT YET FUNCTIONAL", called
  the LLM once, and parked on `sleep(u64::MAX)`.
- Every action parsed its parameters, logged "not yet implemented", and returned `NoAction`.
- `usb_mouse_detached` had no emit site — correctly `#[ignore]`d with that note.

A single `list_devices()` call would have caught all of it. That is the point: a suite that never
speaks the protocol cannot tell a working device from a socket that gets dropped, however many
tests it contains.

## Tests

| Test | Proves | LLM calls |
|---|---|---|
| `test_usb_mouse_moves_clicks_and_scrolls` | devlist advertises 03/01/02 and the device imports; `move_relative(300, -5)` splits into reports whose signed axes sum to exactly (300, -5) with no buttons held; `click` presses bit 0 then releases; `scroll down 2` sends two reports with wheel = -1 | 2 |
| `test_usb_mouse_drag_holds_the_button_throughout` | a drag presses without moving, keeps the button held on **every** intermediate report, covers the full (40, 20) delta, and releases at the end | 2 |
| `test_usb_mouse_detach` | closing the USB/IP session raises `usb_mouse_detached` | 3 |

**LLM budget: 7 calls.** The first test bundles three actions into one attach response, so the
whole pointer vocabulary costs a single call.

The drag assertion is the load-bearing one. "Button held on every intermediate report" is the
difference between a drag and a click-then-move, and it is invisible to any test that does not
decode the reports.

## Synchronisation

Every test waits for `"USB mouse LLM call completed for connection"` before reading reports: the
attach event fires as soon as the TCP connection is accepted, well before `OP_REQ_IMPORT`.
`read_reports` then polls the IN endpoint the way a host does, with a 10s ceiling so a device
that stops producing fails rather than hangs.

`llm_failure_test.rs` waits on `"LLM call failed for USB mouse connection"` and then on
`"decision=fail_closed_llm_error"`. The second wait is not redundant: the wire is silent in
every failure mode, so the tag is the only thing distinguishing a backend outage from a model
that answered with nothing, which makes it part of the contract rather than log decoration.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features usb-mouse \
    --test server -- --test-threads=100 usb_mouse
```

Under two seconds. Needs `libusb-1.0`; not available in Claude Code for Web. **Run it twice**:
the first run after a source edit relinks the `netget` binary the tests spawn.

## Not covered

- A real Linux host: `sudo usbip attach`, `vhci-hcd`, `evtest`.
- `move_absolute`. Its corner-slam strategy (a boot-protocol mouse has no idea where the pointer
  is) is the action most likely to behave differently against a real desktop — pointer
  acceleration, multi-monitor layouts, clamping — and nothing here exercises it.
- Two hosts attached at once, and therefore the multi-candidate branch of `resolve_handler`.
- `GET_REPORT` on the control endpoint.
- Right and middle buttons; only `left` is exercised.
