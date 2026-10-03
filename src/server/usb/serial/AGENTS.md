# USB Serial Server Implementation

## Overview

Virtual USB CDC ACM serial port exported over USB/IP. A Linux host that imports it sees
`/dev/ttyACM0`; anything that speaks USB/IP over TCP can drive it with no kernel module.

**State: Experimental.** The connection handler used to be a single
`error!("... placeholder - full USB/IP integration needed")`, so none of the three events could
fire. It is now implemented and E2E-tested against a real USB/IP client.

## Layout

| File | What it does |
|---|---|
| `mod.rs` | Accept loop, USB/IP session, the three events, connection state machine |
| `handler.rs` | `UsbCdcAcmSerialHandler` — the `usbip::UsbInterfaceHandler` implementation |
| `actions.rs` | Action/event definitions, per-connection handler registry, `execute_action` |

## Why the handler is hand-written

`usbip` 0.9 ships `cdc::UsbCdcAcmHandler`, but it is a demo:

- a host write on the bulk OUT endpoint is `info!`-logged and thrown away, so
  `usb_serial_data_received` could never fire;
- the CDC class requests (`SET_LINE_CODING`, `GET_LINE_CODING`, `SET_CONTROL_LINE_STATE`,
  `SEND_BREAK`) are not handled at all.

So `handler.rs` implements `handle_urb` itself but still takes the CDC functional descriptors
(`get_class_specific_descriptor`) and the endpoint layout (`endpoints()`) from the crate — it is
the authority on what a CDC ACM interface looks like on the wire.

Endpoints, from the crate: interrupt IN `0x81` (notifications, never used), bulk IN `0x82`,
bulk OUT `0x02`, 512-byte packets.

## Wiring the sync handler to the async LLM

`handle_urb` is synchronous and cannot await an LLM call. Host writes are pushed onto an
unbounded channel; the connection task in `mod.rs` receives them and raises
`usb_serial_data_received`. While an LLM call is in flight the task is not reading the channel,
so writes queue up; the next receive coalesces everything pending into a single event rather
than firing one round-trip per URB.

Device-to-host data goes the other way: `send_data` appends to the handler's `tx_buffer`, and
the host drains it on its next bulk IN URB, `min(max_packet_size, transfer_buffer_length)` bytes
at a time.

## The detach path

`handle_connection` does **not** park on `sleep(u64::MAX)` — that is what the rest of the USB
family does, and it is why `usb_*_detached` never fires anywhere else. Here the task selects
over the rx channel and the USB/IP session's `JoinHandle`; when the session ends, the loop
breaks, `usb_serial_detached` is raised, the handler is dropped from the registry and the
connection is closed in `AppState`.

## Signalling dropped data (`SERIAL_STATE` overrun)

When a host write raises `usb_serial_data_received` but the **LLM call fails**, those bytes are
gone and there is no request/response framing to fail — silence would be indistinguishable from a
port with nothing to send. So the server queues a CDC `SERIAL_STATE` notification with `bOverRun`
set (CDC PSTN 1.2 §6.5.4, `bmRequestType 0xA1`, `bNotification 0x20`) on the interrupt IN
endpoint; the host drains it, one whole notification per interrupt IN URB, from
`UsbCdcAcmSerialHandler`'s notification queue.

Only `data_received` earns one — attach has had nothing written to it, and by detach the port is
gone. The mechanism is `SerialProtocol::signal_overrun` → `handler.queue_serial_state_overrun()`,
raised from the connection task in `mod.rs` on the LLM-failure path. `pending_notifications()`
exposes the queue depth for tests. This is currently the **only** serial-state bit the device
ever sets.

## Both buffers are bounded, and each was unbounded in a different direction

**Host to device.** The connection loop coalesces the whole backlog on every event:

```rust
while data.len() < MAX_EVENT_BYTES { ... rx_rx.try_recv() ... }
```

What is being coalesced is precisely what accumulated while the previous LLM call was in
flight, so with no cap an attached host writing at line rate for the length of a model
round-trip decided the size of that buffer — and then of the `from_utf8_lossy` copy of it, and
of the event JSON built from that, all of which the model then reads. `MAX_EVENT_BYTES` is
8 KiB; the excess is reported to the host as a `SERIAL_STATE` overrun (below) rather than
dropped in silence.

**Device to host.** `queue_tx` appended without limit while only the host's bulk IN URBs
drained it, so the model's pace decided how much memory a port held and a host that never polls
never gave any back. `MAX_TX_BUFFER` is 64 KiB — several seconds at 115200 baud, the port's own
default. `send_data` reports the refusal to the model instead of letting it believe bytes went
out; a real UART drops on a full transmit buffer and says so.

## `set_line_coding` is range-checked, not narrowed

`baud_rate` was read as `as_u64()? as u32` and `data_bits` as `as u8`, both silent: 5000000000
became 705032704 and 264 became 8, after which `GET_LINE_CODING` handed the host a
configuration nobody had asked for. Both are checked at full width now, `data_bits` against the
five values CDC PSTN 1.2 table 17 defines (5, 6, 7, 8, 16).

## `LineCoding::from_bytes` is total

It is `pub`, takes an arbitrary `&[u8]`, and used to index `bytes[0..6]` with no length check —
relying entirely on `handle_control` guarding with `req.len() >= 7`. The payload is a
SET_LINE_CODING data stage, so its length is chosen by the USB host; the defect was latent, one
new caller away from a panic on a control OUT with a short or empty body.

It is also the kind of panic that hides: `handle_urb` runs inside a `tokio::spawn`ed connection
task, so the panic is swallowed by the task, the server stays `Running`, the log shows the
transfer succeeding and the peer hangs. (`src/panic_log.rs` now writes it to `netget.log`, which
makes it findable — not fixed.)

It returns `Option<Self>` now, and `LineCoding::WIRE_LEN` names the 7. A short payload leaves
the port's existing coding in place and logs at WARN, which is what a device does when it cannot
honour a SET_LINE_CODING. `tests/server/usb_serial/line_coding_test.rs` asserts every length
below 7 refuses **and** that a well-formed payload still decodes field for field — a guard
returning `None` for everything would satisfy the first half alone.

## LLM Actions

**send_data**: queue text for the host's next read.

```json
{"type": "send_data", "data": "Hello\n"}
```

**set_line_coding**: change what the port reports on `GET_LINE_CODING`.

```json
{"type": "set_line_coding", "baud_rate": 9600, "data_bits": 8, "parity": "none", "stop_bits": 1}
```

`parity` and `stop_bits` are named, not numeric; an unknown value is an error rather than a
silent default. Default line coding is 115200 8N1.

**wait_for_more**: send nothing.

### `connection_id` is optional

Both wire actions take an optional `connection_id`. With exactly one host attached it is
inferred; with several, omitting it is an error naming the candidates. The rest of the USB
family requires the model to copy the id back from the event, which is a reliable source of
wrong answers — and a serial port normally has exactly one host.

## LLM Events

- `usb_serial_attached` — a host connected. Fields: `connection_id`.
- `usb_serial_data_received` — the host wrote. Fields: `connection_id`, `data` (text).
- `usb_serial_detached` — the USB/IP session ended. Fields: `connection_id`. Declared
  `with_no_actions()`: the port is gone, so there is nothing to write to.

## Known limitations

- **Real Linux attach is untested.** `sudo usbip attach -r <host> -b 0-0-0` needs `vhci-hcd` and
  root on the client; the E2E tests speak USB/IP directly over TCP instead.
- **The host changing the baud rate raises no event.** `SET_LINE_CODING` from the host is
  recorded in the handler and logged, but a script or the model cannot react to it.
- **No flow control, and only the overrun serial-state notification.** The interrupt IN endpoint
  now carries a real CDC `SERIAL_STATE` notification — but only `bOverRun` (see *Signalling
  dropped data* below). Break, DCD, DSR, ring and framing errors are still never reported.
- **`data` is text.** Bytes that are not valid UTF-8 are lossily converted before reaching the
  handler, per the project's no-raw-bytes rule for event data.

## Build

```bash
./cargo-isolated.sh build --no-default-features --features usb-serial
```

Needs `libusb-1.0` (the `usbip` crate links it): `brew install libusb pkg-config` on macOS,
`apt-get install libusb-1.0-0-dev pkg-config` on Debian. Not available in Claude Code for Web.

## Testing

```bash
./cargo-isolated.sh test --no-default-features --features usb-serial \
    --test server -- --test-threads=100 usb_serial
```

See `tests/server/usb_serial/CLAUDE.md`.

## USB/IP is screened before the crate sees it

This server does **not** call `usbip::handler` on the accepted socket. It calls
`crate::server::usb::guard::run_guarded_usbip`, which relays the connection into the crate
through an in-memory pipe and decides every inbound USB/IP message first.

The reason is a pre-auth allocation bomb in `usbip` 0.9.0: `USBIP_CMD_SUBMIT` is 48 bytes of
header, and the crate then does `vec![0; transfer_buffer_length as usize]` and
`vec![0; 16 * number_of_packets as usize]` with no bound on either number. USB/IP authenticates
nothing, and the session task is spawned before the attach LLM call, so 48 bytes from a peer
that has not imported anything ask for 4 GiB.

The screen refuses a declared `transfer_buffer_length` over
`guard::MAX_TRANSFER_BUFFER_BYTES` (1 MiB, a NetGet policy choice) and a `number_of_packets`
over `guard::MAX_ISO_PACKETS` (1024, the Linux kernel's own `USBIP_MAX_ISO_PACKETS`), along
with unknown command words, a `direction` that is not a single bit, a non-zero `status`, and a
payload the peer announces and then stalls on. A refusal closes the connection — the screen
sits below the crate and cannot synthesise a `USBIP_RET_SUBMIT` for a sequence the crate never
saw — and logs at ERROR with a `decision=fail_closed_*` tag.

`MAX_TRANSFER_BUFFER_BYTES` is declared in `metadata()` via `.max_inbound_bytes(...)`.
`src/server/usb/CLAUDE.md` has the full reasoning, the constants and what is still unguarded;
`tests/server/usb_msc/guard_test.rs` is the wire-level test.
