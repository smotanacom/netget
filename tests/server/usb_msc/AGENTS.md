# USB Mass Storage Class (MSC) E2E Tests

## What these prove, and what they do not

The tests answer one concrete question: *pretend to be a USB drive and serve a single file
`hello.txt` containing `world`* — does that work, **with the model supplying the contents**?

They drive a **real USB/IP client over TCP** (`tests/helpers/usbip_client.rs`): `OP_REQ_DEVLIST`
and `OP_REQ_IMPORT`, then Bulk-Only Transport CBW/CSW pairs carrying real SCSI commands. The
assertions are on the bytes the host receives.

In the headline test **nothing names a file on disk**. The model answers `usb_msc_attached` with

```json
{"type": "serve_files", "files": [{"name": "hello.txt", "content": "world"}]}
```

and the test then walks the volume the way a host would: read sector 0, parse the BPB to find the
root directory and the data region, find the `HELLO   TXT` entry, follow its first-cluster field
to the right LBA, read it. `world` arriving there can only have come from the mock's response —
which is what makes this a test of the LLM-driven path rather than of a file netget wrote.

The geometry is computed from the served bytes, not from constants shared with the
implementation. A volume laid out wrongly sends the test to the wrong sector and the content
assertion fails. That is deliberate: a host does exactly this walk.

**This is the device side only.** There is no `vhci-hcd`, no `/dev/sdX`, no kernel filesystem
driver — macOS has no USB/IP client, which is why the protocol is spoken directly. A passing run
means netget puts the right bytes on the wire for the right SCSI commands, and that those bytes
are a valid FAT16 volume containing `hello.txt` -> `world`. It does **not** mean Linux mounts the
volume and `cat /mnt/hello.txt` prints `world`. That still needs a real machine with the kernel
module and root, and remains untested.

The tests this replaced connected a bare `TcpStream` and checked that an event fired. That could
not have caught any of what was actually broken — the device was exported on a second listener
bound to the client's address, every SCSI command would have panicked on `block_on`, control
requests were routed to the bulk IN path, and three of the four events could never fire. Three
of the seven tests were `#[ignore]`d with product-gap notes; two more passed while their action
payloads used parameter names the executor rejects.

## The FAT16 builder (`fat16.rs`)

The test fixture, used by the two tests that exercise the **file-backed** mode
(`startup_params.disk_image` and `mount_disk`). It is deliberately a *separate* implementation
from `src/server/usb/msc/fat16.rs`: building an image with the code under test and reading it
back with the same code proves only self-consistency.

Built rather than committed as a binary, so the layout is visible and the bytes reproducible —
every field is fixed, including the volume id and timestamps.

512-byte sectors, 1 sector per cluster, 8192 sectors (4 MiB), which gives 8095 data clusters:
inside FAT16's 4085..65525 window, so a host reads the FAT with 16-bit entries.

| LBA | Contents |
|---|---|
| 0 | Boot sector / BPB |
| 1..33 | FAT #1 |
| 33..65 | FAT #2 |
| 65..97 | Root directory (512 entries) — `fat16::ROOT_DIR_LBA` |
| 97.. | Data; cluster *n* at `fat16::cluster_lba(n)` |

## The client (`tests/helpers/usbip_client.rs`)

Written against the wire format, **not** against the `usbip` crate's types, so it compiles into
every test binary regardless of which protocol features are on.

- `connect` / `list_devices` / `import` — enumeration and attach.
- `submit` / `control_in` / `control_out` / `bulk_in` / `bulk_out` — raw URBs.
- `get_max_lun`, `bulk_only_reset` — the two MSC class requests.
- `scsi_data_in` / `scsi_data_out` / `scsi_no_data` — BOT command wrappers, tag-checked.
- `scsi_inquiry`, `scsi_test_unit_ready`, `scsi_read_capacity_10`, `scsi_read_10`,
  `scsi_write_10`, `scsi_request_sense`, `scsi_mode_sense_6`.

Encoding traps it exists to hide: USB/IP is big-endian while CBW/CSW and USB setup packets are
little-endian; USB/IP direction is `OUT = 0`, `IN = 1` (the opposite of `rusb::Direction`); and
`USBIP_CMD_SUBMIT` carries an endpoint *number*, so an IN transfer on `0x81` is `ep = 1`.

`scsi_data_in` tolerates a device that short-circuits to the CSW where the data phase would have
been — that is what a failed command looks like without an endpoint stall.

## Tests

| Test | Proves | LLM calls |
|---|---|---|
| `test_usb_msc_serves_hello_txt` | devlist advertises 08/06/50; import; Get Max LUN; INQUIRY; the **model's** two files are laid out as a FAT16 volume with the label it chose; the BPB's sector count agrees with READ CAPACITY(10); both directory entries are found with the right sizes and distinct clusters; following `hello.txt`'s cluster yields `world`; a write is refused with DATA PROTECT and does not land; `usb_msc_read` fires | 3 |
| `test_usb_msc_write_and_detach` | `set_write_protect(false)` lets WRITE(10) through; the host reads back what it wrote; the bytes are flushed to the image file on disk; `usb_msc_write` and `usb_msc_detached` fire | 4 |
| `test_usb_msc_mount_then_eject` | `mount_disk` swaps in a different image at its own size and serves it; the read event's handler ejects; TEST UNIT READY then fails NOT READY / MEDIUM NOT PRESENT and READ(10) returns no data | 3 |

**LLM budget: 10 calls**, at the project ceiling. Read events are coalesced, so a burst of
`READ(10)`s may produce one event or several; those rules use `expect_at_least(1)` rather than an
exact count.

## Synchronisation

Every test waits for `"USB MSC LLM call completed (attach)"` before asserting: the attach event follows the peer's `OP_REQ_IMPORT`, not the TCP accept — a bare
connect costs no model call at all (see `src/server/usb/CLAUDE.md` and
`tests/server/usb_keyboard/attach_on_import_test.rs`), so the log line is the only
signal that the import has been answered. The log line puts
the event kind *before* the connection id precisely so a test can wait on one specific event with
a substring match — waiting on a call *count* is ambiguous when a read event and a write event
race.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features usb-msc \
    --test server -- --test-threads=100 usb_msc
```

About 1 second for the suite. **Run it twice**: the first run after a source edit relinks the
`netget` binary the tests spawn, and every test then fails with
`Timeout waiting for netget startup` at exactly 120s.

## Not covered

- Attaching from a real Linux host (`sudo usbip attach`) — needs `vhci-hcd` and root.
- Binary file contents. `serve_files` takes text, so there is nothing to test.
- Rejected 8.3 names, and files that overflow the data region.
- Mounting the volume through a real filesystem driver.
- Multiple hosts attached at once, and therefore the `connection_id`-required branch of
  `resolve_handler`.
- `Bulk-Only Mass Storage Reset` mid-transfer (the client can send it; nothing asserts on it).
- Multi-sector transfers and short data phases.

## `guard_test.rs` — the pre-auth allocation bomb

Separate from the table above, and not part of its 10-call budget: two tests of
`src/server/usb/guard.rs`, the USB/IP message screen every USB protocol now runs the `usbip`
crate behind.

Each sends **48 bytes and nothing else** — a `USBIP_CMD_SUBMIT` header with no payload behind
it — over a bare `TcpStream`, without importing a device first, because the point is that the
crate's `vec![0; transfer_buffer_length as usize]` and `vec![0; 16 * number_of_packets as usize]`
are reachable before any attach and before any model call.

| Test | Declares | Expects |
|---|---|---|
| `test_oversized_transfer_buffer_is_refused_before_allocation` | `transfer_buffer_length = 0xFFFFFFFF` | `decision=fail_closed_oversized_urb`, connection closed |
| `test_oversized_iso_descriptor_count_is_refused` | `number_of_packets = 0xFFFFFFFE` | `decision=fail_closed_oversized_iso`, connection closed |

`0xFFFFFFFE` rather than `0xFFFFFFFF` because that value and `0` are the two the protocol
defines as "not isochronous" and both are exempt by design.

The header is built by hand rather than through `helpers::usbip_client`: the helper only
produces headers whose declared lengths match the bytes it goes on to send, which is exactly the
invariant under attack.

Three things each test asserts, and each is there for a different failure:

- **The `decision=` tag** — the refusal happened, and happened for the stated reason. Without
  the guard no tag appears and the test fails here.
- **The connection closes.** Unguarded, the crate allocates the declared length and then parks
  in `read_exact` waiting for bytes that never arrive, so the socket and the allocation are both
  held; the 20s timeout is what catches that.
- **A fresh client still enumerates the device.** The control. A refusal that killed the process
  or the listener would satisfy both assertions above.

`usb_msc_read`/`usb_msc_write` carry `expect_calls(0)`: a refused URB never reaches
`handle_urb`, so it costs no model budget. That is a claim about price, not the discriminator —
unguarded, the crate never reaches the handler either.

Each test costs 3 LLM calls: startup, the attach call the attacking connection provokes, and
the attach call the control connection provokes. The attach call fires on the peer's first
admitted `OP_REQ_IMPORT`, not on the TCP accept — the same thing line 95 above says — which is
why each test waits for `"USB MSC LLM call completed (attach)"` before sending anything.

(This paragraph said the opposite for a long time: "it fires on TCP accept, not on
`OP_REQ_IMPORT`", contradicting both the code and its own file ninety lines earlier. The
distinction is not cosmetic — if attach really did hang off the accept, a silent peer would
provoke a model call before saying anything, and the first-byte deadline in `usb/guard.rs`
would have to be argued against a peer that might be parked waiting for a human answer. It
does not, which is why 30 seconds is the right number there.)

**Verified by removing the guard**: pointing `msc/mod.rs` back at `usbip::handler(&mut stream,
...)` makes both tests fail, on the missing tag and then on the 20s timeout.
