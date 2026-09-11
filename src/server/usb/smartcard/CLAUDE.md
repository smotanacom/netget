# USB Smart Card (CCID) Server Implementation

## What this is

NetGet exports a **real USB CCID device** — chip card interface device, USB interface class
`0x0B` — over USB/IP. A host that imports it sees a card reader with a card in it; anything
that speaks USB/IP over TCP can drive it with no kernel module and no daemon.

`DevelopmentState` is `Experimental`. `PrivilegeRequirement` is `None`: no device node, no
privileged port, no root.

## Why CCID and not vpicc

The previous implementation used the `vpicc` crate against an external `vpcd` daemon, and it
never ran at all: `Server::spawn` was `bail!("USB Smart Card server not yet implemented")`, so
the protocol was `Incomplete`, hidden from the model, and unreachable. The working code that
did exist (`spawn_with_llm_actions` connecting *out* to `vpcd`) was dead — nothing called it.

CCID over USB/IP became viable when the repo upgraded `usbip` 0.3 → 0.9. 0.3 pinned tokio 0.3,
so `usbip::server()` panicked "there is no reactor running" on every attach and the USB/IP
device path had never once worked. With 0.9 it does, and five sibling protocols
(`usb-keyboard`, `usb-mouse`, `usb-msc`, `usb-fido2`, `usb-serial`) run on it.

CCID is also the *right* layer: a smart card reader is a USB class device, so emulating it as
one removes the external daemon, matches the rest of the USB family, and gives a real host
(`usbip attach` + `pcscd`) a path in that vpcd never had. `vpicc`, `vpcd`, `rsa` and `sha2`
are gone from this module entirely.

## Layout

| File | What it does |
|---|---|
| `mod.rs` | USB/IP accept loop, the four events, the APDU answer path, fail-closed logic |
| `ccid.rs` | CCID rev 1.1 message parsing and building, class descriptor |
| `handler.rs` | `UsbCcidHandler` — the `usbip::UsbInterfaceHandler`, slot state machine |
| `apdu.rs` | ISO 7816-4 command APDU parsing, response APDU building |
| `actions.rs` | Action/event definitions, metadata, `execute_action` |

## The device

- Interface class `0x0B` (`usbip::ClassCode::SmartCard`), subclass `0x00`, protocol `0x00`.
- 54-byte CCID class descriptor (`bDescriptorType 0x21`), `bcdCCID 0x0110`, one slot,
  `dwProtocols` T=0 and T=1, `dwFeatures 0x000204BA` (automatic parameter/voltage/frequency/
  baud/PPS/IFSD handling plus **short APDU level of exchange**), `dwMaxCCIDMessageLength 271`.
- Endpoints: interrupt IN `0x81` (8 bytes, slot-change notifications), bulk IN `0x82`, bulk
  OUT `0x02`, **64-byte packets** as on a full-speed reader — so a long response spans several
  URBs and the host reassembles it.

`dwFeatures` declaring short APDU level of exchange is what makes the design work: the host
hands the reader a whole command APDU rather than TPDU fragments, which is exactly the
granularity a handler can answer at.

## CCID messages

Every command is answered by exactly one response with the same `bSeq`.

| Command | Answered by | Where |
|---|---|---|
| `PC_to_RDR_IccPowerOn` (0x62) | `RDR_to_PC_DataBlock` carrying the ATR | handler.rs |
| `PC_to_RDR_IccPowerOff` (0x63) | `RDR_to_PC_SlotStatus` | handler.rs |
| `PC_to_RDR_GetSlotStatus` (0x65) | `RDR_to_PC_SlotStatus` | handler.rs |
| `PC_to_RDR_XfrBlock` (0x6F) | `RDR_to_PC_DataBlock` carrying the response APDU | **mod.rs, via the handler's answer** |
| `PC_to_RDR_GetParameters` (0x6C) | `RDR_to_PC_Parameters` (default T=0 block) | handler.rs |
| `PC_to_RDR_SetParameters` (0x61) | `RDR_to_PC_Parameters` | handler.rs |
| `PC_to_RDR_ResetParameters` (0x6D) | `RDR_to_PC_Parameters` | handler.rs |
| `PC_to_RDR_Abort` (0x72) | `RDR_to_PC_SlotStatus` | handler.rs |
| `PC_to_RDR_IccClock` (0x6E) | `RDR_to_PC_SlotStatus` | handler.rs |
| `PC_to_RDR_T0APDU` (0x6A) | `RDR_to_PC_SlotStatus` | handler.rs |
| `PC_to_RDR_Escape` (0x6B) | `RDR_to_PC_Escape`, refused `CMD_NOT_SUPPORTED` | handler.rs |
| anything else | `RDR_to_PC_SlotStatus`, failed, `bError` `CMD_NOT_SUPPORTED` | handler.rs |

`RDR_to_PC_NotifySlotChange` (0x50) goes out on the interrupt endpoint whenever the card is
inserted or removed, and on the first poll after attach.

Only `XfrBlock` reaches the handler. Everything else is reader mechanics that no model should
be asked to answer per URB — a `GetSlotStatus` poll loop would otherwise be one LLM call per
poll.

### The slot state machine

`IccPowerOn` fails (`RDR_to_PC_SlotStatus`, `bmCommandStatus` = failed, `bError` = `ICC_MUTE`)
when no card is present. `XfrBlock` fails the same way when no card is present, when the card
has not been powered on, or when it carries an empty payload. **None of those make an LLM
call.** Removing the card also clears the power state, as pulling one out would.

## Wiring the sync handler to the async handler

`handle_urb` is synchronous and cannot await an LLM call, so `XfrBlock` payloads are pushed on
an unbounded channel; the connection task in `mod.rs` receives them, raises
`usb_smartcard_apdu_received`, and queues the `RDR_to_PC_DataBlock` back on the handler for the
host's next bulk IN URB. Card state (`atr`, `card_present`) lives in one
`Arc<std::sync::Mutex<CardState>>` shared by the server and every session, so `handle_urb` can
read it without awaiting.

Responses are queued as **whole CCID messages**, not a byte stream. A message longer than the
URB is split at the limit and the remainder pushed back to the front of the queue, so 64-byte
packets reassemble correctly while two queued messages are never merged into one transfer —
a host parses one CCID message per transfer.

## Events — all four fire

| Event | Fires when | Actions |
|---|---|---|
| `usb_smartcard_reader_ready` | after `bind()`, before the first host is accepted | `set_atr`, `set_card_present` |
| `usb_smartcard_attached` | a host completed `OP_REQ_IMPORT` | `set_atr`, `set_card_present` |
| `usb_smartcard_apdu_received` | per `PC_to_RDR_XfrBlock` | `respond_to_apdu` |
| `usb_smartcard_detached` | the USB/IP session ended | `with_no_actions()` |

The ready event is awaited **before** the accept loop starts, so a host can never power up a
card whose ATR has not been applied yet.

## Event data is structured, not a byte blob

`usb_smartcard_apdu_received` carries `ins_name` ("SELECT_BY_AID", "VERIFY", "READ_BINARY", …),
`cla`/`ins`/`p1`/`p2` as two-hex-digit strings, `lc` and `le` as numbers, `application_id` for
a SELECT by DF name, and the command data as **`data_hex` plus `data_text`** — `data_text` only
when every byte is printable ASCII. Hex is used for `data_hex` deliberately and is called out
in the parameter description: the data field of an arbitrary APDU is opaque bytes chosen by the
host and there is no structured form to offer.

`card_type` (the startup parameter) is reported in every event so a handler can branch on it.
The server does not interpret it.

## Responding

`respond_to_apdu` takes an optional body plus a **required** two-byte status word:

```json
{"type": "respond_to_apdu", "data_text": "NetGet PIV", "sw1": "90", "sw2": "00"}
{"type": "respond_to_apdu", "data_hex": "6F0A8408A000000308000010", "sw1": "90", "sw2": "00"}
{"type": "respond_to_apdu", "sw1": "69", "sw2": "82"}
```

`data_text` and `data_hex` are **mutually exclusive** and supplying both is an error — the same
lesson as `send_tcp_data`: `"48656c6c6f"` is simultaneously valid text and valid hex and only
the sender knows which it meant, so the encoding is declared rather than sniffed.
`execute_action` normalises `data_text` to hex, so the server has exactly one form to decode,
and every hex field (`atr_hex`, `data_hex`, `sw1`, `sw2`) is decoded where the action is
executed — a malformed value is an error, never something logged as if accepted. `parse_hex`
tolerates the spaces a model naturally writes between bytes and rejects everything else.

`set_atr` and `set_card_present` are applied from the results of `usb_smartcard_reader_ready`
and `usb_smartcard_attached`. `set_card_present` also raises a slot-change notification on the
interrupt endpoint.

## Fail closed

If the handler errors, returns no `respond_to_apdu`, or returns one that cannot be decoded, the
card answers **`6F00`** (ISO 7816-4 "no precise diagnosis") and logs at ERROR with a `decision=`
tag. It never falls through to `9000`. The model's own refusal (`6982`, `6A82`, `6D00`, …) is
therefore structurally distinguishable from the model having said nothing — the OAuth2 failure
mode in the root CLAUDE.md, avoided by construction. A malformed command APDU is answered `6700`
locally and never reaches the handler, so garbage cannot be used to drive LLM calls.

**"It never falls through to `9000`" was false when this section first claimed it**, and the
gap is worth keeping because the sentence read as a guarantee. `sw1` and `sw2` defaulted to
`"90"` and `"00"` — in `execute_action` *and* again in `decode_status_byte`, so removing one
would not have been enough — which meant
`{"type": "respond_to_apdu", "data_text": "AUTH OK"}` put `41555448204F4B9000` on the wire. The
handler had named no status word at all and the card reported success anyway. Only the third
case above (an *absent* `respond_to_apdu`) actually failed closed; a present one with a missing
field did not, and that is the likelier model output of the two.

Both defaults are gone and both parameters are declared `required: true`, so nothing between
the model's answer and the wire can invent a status word. The reasoning is `eapol`'s: success
must be spelled out rather than reachable by omission. It matters more here than the shape
alone suggests, and for the opposite reason to the obvious one — this server holds no PIN and
no key (see *No built-in card logic* below), so **the model is the access control** and there
is no second gate behind it. `src/server/nfc/` had the identical defect in the identical two
layers; the tag and the card were fixed the same way.

`tests/server/usb_smartcard/e2e_test.rs` pins it with an INTERNAL AUTHENTICATE case. Restore
either default and it fails with `41555448204F4B9000` on the wire — the test was verified that
way round, so it is known to catch the original defect rather than merely to agree with the fix.

The card always answers *something*, so a host is never left waiting on its own timeout.

## No built-in card logic

There is no file system, no PIN store and no key store. The only state the server holds is what
the handler put there (`atr`, `card_present`) plus the `card_type` startup parameter. The
previous implementation had a hardcoded MF/EF hierarchy, a PIN of `123456`, and an RSA-2048 key
with `INTERNAL AUTHENTICATE` signing built in — that is storage and card logic implemented
inside a protocol, which the root CLAUDE.md forbids. `crypto.rs` was deleted with it.

## Hostile input

Everything on the bulk OUT endpoint is attacker-controlled.

- `CcidCommand::parse` checks the 10-byte header is present, refuses a `dwLength` larger than
  the advertised `dwMaxCCIDMessageLength`, and refuses a truncated payload. A malformed message
  has no trustworthy `bSeq`, so it is logged and dropped rather than answered at a guess.
- `ApduCommand::parse` bounds-checks every index; short and extended forms are both handled;
  truncated, over-long and inconsistent APDUs are `Err`, never a panic. A panic inside a URB
  callback would be silent while the server still reported `Running`.
- Nothing allocates on a host-supplied length before checking it.
- Both mutexes are recovered from poisoning rather than propagating it, so one panicking task
  cannot take the reader down for the rest of the process.

## No connection state machine, deliberately

Other protocols hand-roll Idle → Processing → Accumulating to stop two LLM calls racing on one
connection and to reassemble partial reads. Neither problem exists here: the CCID header
carries its own length, so there is nothing to reassemble, and each `XfrBlock` is answered
before the next is taken off the channel.

## Startup parameters

- `card_type` — `piv`, `openpgp`, `generic` (default). Reported to the handler in every event.
  Declared and read; the server does not interpret it.

That is the whole list. `default_pin`, `vpcd_host` and `vpcd_port` are gone with the vpcd
design.

## Known limitations

- **Real Linux attach is untested.** `sudo usbip attach -r <host> -b 0-0-0` plus `pcscd` needs
  `vhci-hcd` and root on the client; the E2E tests speak USB/IP directly over TCP instead.
- **Short APDU level only.** CCID messages are capped at the advertised 271 bytes, so a
  response body above 259 bytes is refused rather than chained. There is no `GET RESPONSE`
  bookkeeping and no command chaining — the handler sees what the host sent and answers it.
- **No time extension.** While the handler thinks, the reader sends nothing; a real reader
  would send `RDR_to_PC_DataBlock` with `bmCommandStatus` = time extension. A host with a short
  BWT and a slow model could give up.
- **No T=0/T=1 transmission layer and no PPS.** Parameters are always reported as the ISO
  7816-3 defaults, whatever the host sets.
- **One card across sessions.** `CardState` is per server, not per connection, so two attached
  hosts share the slot. That matches "one reader, one card" but is worth knowing.
- **Async actions only apply from a smart card event.** `set_atr` / `set_card_present` invoked
  outside `usb_smartcard_reader_ready` / `usb_smartcard_attached` produce a result nothing
  applies. Same shape as `nfc`'s `set_atr`.
- The `usb-smartcard` Cargo feature still pulls `rsa` and `sha2`. Neither is used any more;
  removing them is a `Cargo.toml` change outside this module.

## Build

```bash
cargo check --no-default-features --features usb-smartcard
```

Needs `libusb-1.0` (the `usbip` crate links it): `brew install libusb pkg-config` on macOS,
`apt-get install libusb-1.0-0-dev pkg-config` on Debian. Not available in Claude Code for Web.

## Testing

```bash
CARGO_TARGET_DIR=/tmp/ccid-target cargo test --no-default-features --features usb-smartcard \
    --test server -- --test-threads=100 usb_smartcard
```

See `tests/server/usb_smartcard/CLAUDE.md`.

## References

- USB CCID rev 1.1: https://www.usb.org/sites/default/files/DWG_Smart-Card_CCID_Rev110.pdf
- ISO/IEC 7816-3 — electrical interface and transmission protocols (ATR)
- ISO/IEC 7816-4 — APDU structure and status words
- PC/SC Workgroup: https://pcscworkgroup.com/
- OpenSC: https://github.com/OpenSC/OpenSC
