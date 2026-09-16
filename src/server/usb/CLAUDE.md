# The USB device family (USB/IP)

Six virtual USB devices, each its own Cargo feature, all sharing one transport: USB/IP over
TCP, spoken by the `usbip` 0.9 crate on the socket NetGet accepted. `common.rs` and
`descriptors.rs` hold the shared constant tables and descriptor builders; `guard.rs` holds the
message screen described below. Everything else is per-protocol:

| feature | directory | device |
|---|---|---|
| `usb-keyboard` | `keyboard/` | HID keyboard |
| `usb-mouse` | `mouse/` | HID mouse (boot protocol) |
| `usb-serial` | `serial/` | CDC ACM serial port |
| `usb-msc` | `msc/` | Mass storage (SCSI over Bulk-Only Transport) |
| `usb-fido2` | `fido2/` | FIDO2 / U2F security key (CTAPHID) |
| `usb-smartcard` | `smartcard/` | CCID smart card reader |

The separate `usb` feature is the USB *client* (`nusb`) in `src/client/usb`, not a server.

## The USB/IP message screen — `guard.rs`

**`usbip` 0.9.0 allocates from two numbers the peer supplies and bounds neither.** In
`UsbIpCommand::read_from_socket`, a `USBIP_CMD_SUBMIT` is a 48-byte header followed by

```rust
let mut data = vec![0; transfer_buffer_length as usize];   // peer-supplied u32
socket.read_exact(&mut data).await?;
```

and, immediately after, `vec![0; 16 * number_of_packets as usize]` — up to ~68 GiB on a 64-bit
host, wrapping on a 32-bit one. USB/IP authenticates **nothing**: `OP_REQ_IMPORT` carries a bus
id and no credential, and every protocol here spawns its session task *before* the attach LLM
call, so 48 bytes from a peer that has not even imported a device ask for 4 GiB, pre-auth and
pre-model.

`usbip::handler` is generic over `T: AsyncReadExt + AsyncWriteExt + Unpin`, so — unlike
`nfsserve`, the precedent in `src/server/nfs/guard.rs` — no second listener is needed. NetGet
keeps the real socket, hands the crate a `tokio::io::duplex` pipe, and copies only admitted
messages into it. **Every one of the six protocols goes through
`guard::run_guarded_usbip(stream, server, device, peer, status_tx)`; none of them calls
`usbip::handler` directly.**

The screen reads one whole USB/IP message at a time and decides it from the numbers the peer
*announced*, before the bytes they describe are read and before any arithmetic on them:

| bound | value | why |
|---|---|---|
| `MAX_TRANSFER_BUFFER_BYTES` | 1 MiB | **A NetGet policy choice, not a spec figure** — the field is a bare `u32` and the protocol names no ceiling. HID reports are 4-8 bytes, CTAPHID frames 64, CCID messages a few hundred; the largest legitimate transfer here is an MSC data phase, well inside this |
| `MAX_ISO_PACKETS` | 1024 | This one *is* the number other implementations use: the Linux kernel's own USB/IP stub defines `USBIP_MAX_ISO_PACKETS` as 1024 in `drivers/usb/usbip/usbip_common.h` and rejects more on the submit path in `stub_rx.c`. No device here declares an isochronous endpoint, so a working session only ever sends the two exempt values `0` and `0xFFFFFFFF` |
| `URB_BODY_TIMEOUT` | 30s | An announced payload that then stalls. Only the payload is bounded — never the wait for the *next* message, because an attached device with nothing being asked of it is idle by design |

Two more things are refused, and both are the same class of defect one level down: the crate
`debug_assert!`s that `direction` is a single bit and that an operation-code message's `status`
is zero, so in any debug or test build either one is a **panic raised from the wire, pre-auth**,
swallowed by `tokio::spawn` while the peer hangs. The screen refuses both. An unknown command
word is refused too, so it is the screen and not the crate that decides what is USB/IP.

**A refusal closes the connection, and that is a real limitation rather than the only option.**
`USBIP_RET_SUBMIT` has a `status` field that could carry `-EINVAL`, but a well-formed reply also
has to echo the `seqnum` and `devid` of a URB the crate never saw, into a reply stream the crate
owns and multiplexes. Writing one from underneath would interleave. So the screen closes and the
log carries the reason: an ERROR with a stable tag —
`decision=fail_closed_oversized_urb`, `_oversized_iso`, `_unknown_command`, `_unknown_version`,
`_invalid_direction`, `_nonzero_status`, `_urb_stalled` — in the shape `src/server/radius/`
established.

Each protocol declares the bound to the model and the operator with
`.max_inbound_bytes(crate::server::usb::guard::MAX_TRANSFER_BUFFER_BYTES)` in its `metadata()`.

`tests/server/usb_msc/guard_test.rs` drives both bombs from the wire, and both were verified by
removing the guard from `msc/mod.rs`: unguarded, the 4 GiB allocation happens, no refusal is
logged, and both tests fail on the missing `decision=` tag.

### What is still unguarded

- **Only framing is screened, not USB semantics.** An admitted URB still reaches the protocol's
  own `UsbInterfaceHandler`, which decides whether the endpoint, the setup packet and the
  payload make sense. A 1 MiB URB on an 8-byte interrupt endpoint is the handler's problem.
- **Nothing caps concurrent connections.** `src/server/nfs/guard.rs` has
  `MAX_CONCURRENT_CONNECTIONS` through `crate::server::accept_bounded`; the USB accept loops do
  not, so *n* peers each hold a session, a task and up to `MAX_TRANSFER_BUFFER_BYTES`.
- **There is no first-message deadline.** A peer can connect and say nothing, which costs a
  socket, a task, an `AppState` entry and — because the attach LLM call fires on accept, not on
  import — one model round-trip. That last part is the cheapest amplification left here.
- **Nothing bounds how *many* messages a peer sends.** Each is individually small and the
  screen holds none of them, but a peer can submit URBs as fast as it likes and each admitted
  one is work for the protocol's handler — and, where the handler raises an event, a model call.
