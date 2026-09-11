# USB Mass Storage Class (MSC) Server Implementation

## Overview

A virtual USB flash drive exported over USB/IP. The device presents a Mass Storage interface
(class 0x08, subclass 0x06 SCSI transparent, protocol 0x50 Bulk-Only Transport) and answers a
SCSI-2 subset. A Linux host that imports it sees `/dev/sdX`; anything that speaks USB/IP over TCP
can read and write sectors with no kernel module.

**The protocol owns the drive; the model owns the contents.** That split is the point: netget
does BOT, SCSI, geometry and the FAT16 layout, and the LLM says what files are on it.

**State: Experimental**, but now actually exercised — see Testing.

## Layout

| File | What it does |
|---|---|
| `mod.rs` | Accept loop, USB/IP session, the four events, connection state machine |
| `handler.rs` | `UsbMscHandler` — the `usbip::UsbInterfaceHandler`, BOT state machine, SCSI |
| `disk.rs` | `DiskImage` — sector I/O over a `Vec<u8>` (default) or a memory-mapped file |
| `fat16.rs` | Lays an LLM-supplied file map out as a FAT16 volume, in memory |
| `actions.rs` | Action/event definitions, per-connection handler registry, `execute_action` |

## Three things that were broken and are worth remembering

**1. The device was never reachable.** `handle_connection` dropped the accepted socket and
called `usbip::server(remote_addr, …)`, which tries to *bind* a fresh listener on the client's
own address. It now runs `usbip::handler(&mut stream, server)` on the socket netget already
accepted, exactly as `src/server/usb/serial/mod.rs` does.

**2. Every SCSI command would have panicked.** `usbip` calls the synchronous
`UsbInterfaceHandler::handle_urb` from inside an async fn on a tokio worker, and the handler
used `tokio::runtime::Handle::current().block_on(...)` for each command — "Cannot block the
current thread from within a runtime". The whole SCSI path is now synchronous and the disk sits
behind a `std::sync::Mutex`. The same applied to `execute_action`, whose handler registry was a
tokio `Mutex` reached through `block_on`; it is a `std::sync::Mutex` now, and no guard is held
across an `.await`.

**3. Control requests went to the bulk IN path.** The dispatch tested `endpoint.address == 0`,
but the crate's control **IN** endpoint is `0x80`. `Get Max LUN` was answered with whatever the
BOT state machine happened to have queued. Use `endpoint.is_ep0()` and `endpoint.direction()`.

Endpoints are bulk IN `0x81` and bulk OUT `0x01` — endpoint 1 in both directions, as a real BOT
device presents itself. (It used to be `0x81`/`0x02`, which forced a host to use different
endpoint numbers per direction.)

## BOT state machine

`handle_urb` on bulk OUT decides by *phase*, not by length:

1. `pending_write` set → this is the data phase of a WRITE(10).
2. `csw_pending` set → the command already failed and the host is still pushing the data it
   promised in the CBW. A real device stalls here; USB/IP has no stall, so the bytes are
   discarded and the CSW reports them as residue.
3. Otherwise → parse a CBW.

Deciding by `data.len() == 31` misreads a 31-byte data payload as a command, and made a
write-protected WRITE(10) fail the *transport* rather than the command.

Bulk IN hands out `pending_data` capped at the URB's `transfer_buffer_length`, then the CSW.
The CSW carries a real residue (`expected_transfer - transferred`) and the status the SCSI layer
decided, not a status re-derived from whatever sense happens to be set.

## SCSI commands

INQUIRY, TEST UNIT READY, READ CAPACITY(10), READ(10), WRITE(10), REQUEST SENSE, MODE SENSE(6),
PREVENT/ALLOW MEDIUM REMOVAL, READ FORMAT CAPACITIES. Anything else gets ILLEGAL REQUEST /
INVALID COMMAND OPERATION CODE.

Every CDB is length-checked before its fields are read; a short CDB is INVALID FIELD IN CDB, not
a panic. `DiskImage` bounds-checks with `checked_add` so a host-supplied `lba + count` cannot
wrap and slice out of the mapping.

**Eject is a state, not a sense code.** `eject_disk` used to only `set_sense(NOT_READY)`, which
the next command cleared — an "ejected" device kept serving sectors. There is now a
`medium_present` flag; every command that needs the medium fails NOT READY / MEDIUM NOT PRESENT
while it is clear, and REQUEST SENSE re-arms the condition after reporting it. INQUIRY still
answers, because that is how a host learns the device exists.

## The medium: in memory by default

`usb-msc` used to violate the project's no-storage rule outright. `DiskImage::open_or_create`
memory-mapped a real file, and the default path was `./tmp/netget_msc_disk.img` relative to the
process's working directory. **The protocol was the storage**: the sectors a host read came from
a file netget created, not from the model, and `usb_msc_read` / `usb_msc_write` were decorative —
notifications about data nobody had been asked for. Its own docs called this out as "awkward".

Now:

- **Default (no `disk_image`)**: an empty in-memory FAT16 volume, 8192 sectors / 4 MiB. Nothing
  touches the filesystem and nothing outlives the server. The model fills it by answering
  `usb_msc_attached` with `serve_files`.
- **Opt-in (`startup_params.disk_image`, or `mount_disk`)**: a memory-mapped file the caller
  **named**. That is host state the model refers to — the same shape as `proxy`'s
  `ca_export_path` — rather than storage the protocol invented.

`DiskImage::open_or_create` **keeps the size of an image that already exists**; `default_size_mb`
(10) applies only when creating one. It used to `set_len` to 10 MB unconditionally, which
silently truncated or padded a prepared filesystem image. A partial trailing sector is rounded up
so the mapping is a whole number of sectors, and `in_memory` does the same.

## The FAT16 layout (`fat16.rs`)

512-byte sectors, one sector per cluster, 8192 sectors by default:

| LBA | Contents |
|---|---|
| 0 | Boot sector / BPB |
| 1..33 | FAT #1 |
| 33..65 | FAT #2 |
| 65..97 | Root directory (512 entries) |
| 97.. | Data; cluster *n* at `97 + n - 2` |

The size is chosen so the **cluster count** lands in FAT16's 4085..65525 window, and
`build_volume` refuses anything outside it. That window is the definition of FAT16 — below it a
host must read the FAT with 12-bit entries, above it with 32-bit ones — so a volume that misses
it is not "a small FAT16", it is a volume the host decodes with the wrong entry width.

Names are 8.3 and are **rejected rather than truncated**. A model that asks for
`configuration.json` and silently gets `CONFIGUR.JSO` has been lied to, and the file it then
describes to the user does not exist.

Content is **text**, not base64. A model cannot reliably produce or read base64, and the project
rule forbids raw bytes in an action payload — so a binary payload simply cannot be expressed
here. That is a real limitation, not an oversight.

## LLM Actions

**serve_files** — say what the drive contains. The one to reach for.

```json
{"type": "serve_files",
 "files": [{"name": "hello.txt", "content": "world"}],
 "volume_label": "NETGET"}
```

netget builds a FAT16 volume from that and mounts it. `write_protect` defaults to **true**: the
contents are the model's answer, and a host write would be editing it.

**mount_disk** — serve a host file instead. Opt-in, for a prepared image. An existing image is served at its own size; a missing one is
created (`size_mb`, default 10). `write_protect` defaults to **true**, matching how the device
starts up; a model that wants writes must say so.

```json
{"type": "mount_disk", "disk_image": "/path/to/disk.img", "write_protect": false}
```

**eject_disk** — take the medium away. **set_write_protect** — toggle DATA PROTECT on WRITE(10).
**wait_for_more** — do nothing.

### `connection_id` is optional

Every action takes an optional `connection_id`. With exactly one host attached it is inferred;
with several, omitting it is an error naming the candidates. Three forms are accepted — the
number, the number as a string, and the `conn-N` form the **events themselves carry**. That last
one matters: events report `connection_id.to_string()`, which is `"conn-2"`, not `2`, so a model
quoting the event's own field back would otherwise never match. (The same defect made every
`usb-keyboard` action fail at runtime; see that protocol's notes.)

## LLM Events

- `usb_msc_attached` — a host connected. Fields: `connection_id`, `remote_addr`,
  `total_sectors`, `capacity_mb`. Answer with `serve_files` to give the drive contents.
- `usb_msc_read` — the host read sectors. Fields: `connection_id`, `lba`, `sector_count`,
  `bytes_read`.
- `usb_msc_write` — the host wrote sectors. Same shape, `bytes_written`.
- `usb_msc_detached` — the USB/IP session ended. Declared `with_no_actions()`.

Read and write events are **notifications**: the transfer is already served from the image, and
the data path never waits on the LLM. `handle_urb` is synchronous, so the handler reports
transfers on a channel and the connection task raises the events — the same seam the serial
port uses.

The connection task selects over that channel and the USB/IP session's `JoinHandle`; when the
session ends it raises `usb_msc_detached`, drops the handler from the registry and closes the
connection in `AppState`. It used to park on `sleep(u64::MAX)` after the attach call, which is
why three of the four events could never fire.

**Coalescing matters here.** A host mounting a filesystem issues hundreds of READ(10)s. Whatever
is queued when the task wakes is folded into one read event and one write event; while an LLM
call is in flight the task is not draining the channel, so a burst does not become a burst of
round-trips.

## Sector arithmetic is `usize`, and the drive size is bounded

`read_sectors`, `write_sectors` and `zero_sectors` each computed their byte offset as
`(lba * self.bytes_per_sector) as usize` — a `u32` multiply that overflows at exactly 4 GiB,
which is well inside what `mount_disk` accepted. In a debug or test build that panicked inside
a URB callback, where `tokio::spawn` swallows it and the server goes on reporting `Running`; in
release it wrapped to a small offset and served the wrong sectors as though they were the right
ones. The `lba.checked_add(count)` bounds check above it was already correct and did not help,
because the overflow is in the multiply that follows. All three compute in `usize` now; the LBA
itself stays 32 bits, which is what SCSI READ(10) defines.

`mount_disk`'s `size_mb` was only checked to fit in `u32`, so 4294967295 was accepted: a 4 PiB
`set_len` whose sector count then had to be truncated by `as u32` to be storable, leaving the
device advertising a capacity with no relation to the mapping behind it. It is bounded by what
a 32-bit LBA can address at 512 bytes per sector, checked before anything touches the
filesystem, and `DiskImage::open_or_create` refuses rather than truncates if it is ever reached
with more.

## Known limitations

- **`serve_files` content is text only.** No binary files, and no way to express one.
- **Real host attach is untested.** `sudo usbip attach -r <host> -b 0-0-0` needs `vhci-hcd` and
  root on the client; macOS has no USB/IP client at all. The E2E tests speak USB/IP directly.
- **Single LUN.** `Get Max LUN` answers 0 and the CBW's LUN field is ignored.
- **No hot-swap notification.** `mount_disk` resets BOT state but raises no UNIT ATTENTION, so a
  host that already probed capacity will not re-read it.
- **Write data is not length-checked against the CBW.** A host that sends fewer bytes than
  WRITE(10) asked for has them written and the command completes.

## Build

```bash
./cargo-isolated.sh build --no-default-features --features usb-msc
```

Needs `libusb-1.0` (the `usbip` crate links it): `brew install libusb pkg-config` on macOS,
`apt-get install libusb-1.0-0-dev pkg-config` on Debian. Not available in Claude Code for Web.

## Testing

```bash
./cargo-isolated.sh test --no-default-features --features usb-msc \
    --test server -- --test-threads=100 usb_msc
```

See `tests/server/usb_msc/CLAUDE.md`.

## References

- **USB MSC Bulk-Only Transport**: https://www.usb.org/sites/default/files/usbmassbulk_10.pdf
- **SCSI Commands Reference**: https://www.t10.org/ftp/t10/document.05/05-344r0.pdf
- **USB/IP Protocol**: https://docs.kernel.org/usb/usbip_protocol.html
- **jiegec/usbip crate**: https://github.com/jiegec/usbip
