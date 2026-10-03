# USB Serial E2E Testing

## Strategy

The tests are a **real USB/IP client** written against the protocol, not a bare `TcpStream`
that only causes an accept. They speak `OP_REQ_IMPORT`, then push `USBIP_CMD_SUBMIT` URBs on
the bulk OUT and bulk IN endpoints and on endpoint 0, and assert on the bytes the device
returns.

This is deliberate. The server this replaced registered the connection and then logged
`"placeholder - full USB/IP integration needed"`. A test that merely opened a TCP connection and
checked that an event fired would have been the only thing between a stub and a green suite —
the old tests were exactly that, and were all `#[ignore]`d as a result.

No `usbip` kernel module, no root, no `/dev/ttyACM0`. Everything is loopback TCP.

## The client (`UsbIpClient` in `e2e_test.rs`)

- `attach(port)` — `OP_REQ_IMPORT` for bus id `0-0-0`, reads the 320-byte `OP_REP_IMPORT`.
- `write_serial(bytes)` — bulk OUT URB on endpoint 2, direction 0.
- `read_serial()` — bulk IN URB on endpoint 2, direction 1; returns whatever is queued now.
- `read_serial_until_data(timeout)` — polls `read_serial` like a real host does.
- `get_line_coding()` — control IN on endpoint 0, `bmRequestType 0xA1`, `bRequest 0x21`.

**Wire direction is not rusb's `Direction`.** On the USB/IP wire OUT is 0 and IN is 1;
`rusb::Direction` has `In = 0`. The `usbip` crate keeps a private enum for the wire values and
the two must not be mixed up.

## Tests

| Test | Proves | LLM calls |
|---|---|---|
| `test_usb_serial_attach_and_send_data` | `usb_serial_attached` fires; `send_data` reaches the host | 2 |
| `test_usb_serial_echo` | host write raises `usb_serial_data_received`; the answer comes back | 3 |
| `test_usb_serial_line_coding` | `set_line_coding` changes what `GET_LINE_CODING` reports | 2 |
| `test_usb_serial_detach` | closing the session raises `usb_serial_detached` | 3 |

**LLM budget: 10 calls**, at the project ceiling.

`test_usb_serial_echo` also asserts that `wait_for_more` on attach puts *nothing* on the wire,
so a test cannot pass by the device happening to emit something.

## Synchronisation

Every test waits for `"USB serial LLM call completed for connection"` before asserting:
the attach event follows the peer's `OP_REQ_IMPORT`, not the TCP accept — a bare
connect costs no model call at all (see `src/server/usb/CLAUDE.md` and
`tests/server/usb_keyboard/attach_on_import_test.rs`), so the log line is the only
signal that the import has been answered.

`test_usb_serial_detach` waits for that line **twice** (attach, then detach) via
`wait_for_log_count`, plus the `"USB serial host detached on connection"` line.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features usb-serial \
    --test server -- --test-threads=100 usb_serial
```

Runtime is about 1 second for the whole suite.

## Not covered

- Attaching from a real Linux host (`sudo usbip attach`) — needs `vhci-hcd` and root.
- Multiple hosts attached at once, and therefore the `connection_id`-required branch of
  `resolve_handler`.
- The host sending `SET_LINE_CODING` or `SET_CONTROL_LINE_STATE` (handled, but no event to
  assert on).

## `line_coding_test.rs` — `LineCoding::from_bytes` is total

Four tests, **zero LLM calls**: the decoder is called directly.

`from_bytes` is `pub`, takes an arbitrary `&[u8]`, and used to index `bytes[0..6]` unchecked,
relying on its single call site guarding with `req.len() >= 7`. The payload is a SET_LINE_CODING
data stage whose length the USB host chooses, and a panic inside the spawned connection task
would be swallowed: server `Running`, log showing success, peer hung.

- every length from 0 to 6 returns `None` rather than panicking — this fails with the check
  removed, with `index out of bounds: the len is 0 but the index is 0`;
- **the control**: a well-formed payload still decodes field for field, little-endian baud rate
  included, for two different codings. A guard returning `None` for everything would pass the
  first test alone;
- a longer payload reads only the first 7 octets, as a device does with an over-long data stage;
- `to_bytes` / `from_bytes` still round-trip.

