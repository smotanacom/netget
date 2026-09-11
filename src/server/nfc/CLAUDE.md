# NFC (Near Field Communication) Virtual Tag Server

## What this is

NetGet emulates an **NFC Forum Type 4 tag**. A Type 4 tag is defined as ISO 7816-4 APDU
exchange over ISO-DEP, so the tag's entire behaviour is "answer command APDUs" — and that
is something an LLM can do, given a transport.

`DevelopmentState` is `Experimental`. `PrivilegeRequirement` is `None`: no reader, device
node or privileged port is touched.

## Transport: vpcd framing over a bound TCP socket

NetGet **binds a TCP socket** and speaks the vsmartcard `vpcd` wire format:

```text
reader → tag:  u16 big-endian length | payload
tag → reader:  u16 big-endian length | payload
```

A payload of exactly **one byte** is a vpcd control code; anything longer is a command APDU.

| Code | Meaning | Tag's answer |
|------|---------|--------------|
| `00` | power off | none (acknowledged by silence) |
| `01` | power on | none |
| `02` | reset | none |
| `04` | request ATR | one frame containing the configured ATR |

### Why this transport and not PC/SC

A PC/SC reader cannot be driven into card-emulation mode — the API has no such call, and the
hardware (ACR122U and everything like it) only reads. The previous implementation "used PC/SC"
in name only: it never called the library, bound nothing, and emitted exactly one event.
Emulating the *tag* needs no RF at all, so the tag is exposed over a socket instead.

### Why vpcd framing and not an ad-hoc one

1. **In-repo precedent.** `src/server/usb/smartcard/` already speaks exactly this framing
   (as a vpcd *client*), so the format is not a new invention in this codebase.
2. **It is a real interop path.** `vpcd`, the vsmartcard ifdhandler, can be configured as a
   TCP *client* — `DEVICENAME /dev/null:<host>:<port>` in `/etc/reader.conf.d/vpcd` — in
   which case it connects out to a listening virtual card. A host running `pcscd` then sees
   this server as a PC/SC reader with a card in it. **This direction has not been tested
   against a real `pcscd`**; it is why the framing was chosen, not a claim that it works.
3. **It is trivially drivable without hardware.** A test writes `u16 len + APDU` and reads
   `u16 len + response`. That is the whole client.

The alternative — bare APDUs with no framing — cannot express "the reader powered the card
up and wants the ATR", and cannot delimit two APDUs written in one TCP segment.

## Events — all three fire

| Event | Fires when | Actions |
|-------|-----------|---------|
| `nfc_server_started` | after `bind()`, before the first reader is accepted | `set_atr`, `set_ndef_message` |
| `nfc_tag_selected` | reader sends `SELECT` by DF name: INS `A4`, P1 `04`, non-empty data | `respond_to_apdu` |
| `nfc_apdu_received` | every other command APDU, including `SELECT` by file identifier | `respond_to_apdu` |

Exactly one event — and therefore one handler call — happens per command APDU. A SELECT by
AID does *not* also raise `nfc_apdu_received`.

The startup event is awaited **before** the accept loop starts, so a reader can never reach
a tag whose ATR and NDEF records have not been applied yet.

## Event data is structured, not a byte blob

`nfc_apdu_received` carries `ins_name` ("READ_BINARY", "VERIFY", …), `cla`, `ins`, `p1`, `p2`
as two-hex-digit strings, `lc` and `le` as numbers, and the command data as **`data_hex`
plus `data_text`** — `data_text` is present only when every byte is printable ASCII. Hex is
used for `data_hex` deliberately and is called out in the parameter description: the command
data field of an arbitrary APDU is opaque bytes chosen by the reader, and there is no
structured form of it to offer. The raw whole-APDU hex blob the old implementation advertised
(`apdu_hex`) is gone — it was redundant with the parsed fields.

Both APDU events also carry `tag_type`, `uid`, and the `ndef_records` the handler configured
at startup, so `set_ndef_message` is not write-only: the handler gets its own records back and
can serve them from `respond_to_apdu`.

## Responding

`respond_to_apdu` takes a body plus a two-byte status word:

```json
{"type": "respond_to_apdu", "data_text": "Hello NFC!", "sw1": "90", "sw2": "00"}
{"type": "respond_to_apdu", "data_hex": "D2760000850101", "sw1": "90", "sw2": "00"}
{"type": "respond_to_apdu", "sw1": "69", "sw2": "82"}
```

`data_text` and `data_hex` are **mutually exclusive** and supplying both is an error. This is
the same lesson as `send_tcp_data`: `"48656c6c6f"` is simultaneously valid text and valid hex
and only the sender knows which it meant, so the encoding is declared rather than sniffed.
`execute_action` normalises `data_text` to hex, so the server has exactly one form to decode,
and every hex field (`atr_hex`, `data_hex`, `sw1`, `sw2`) is decoded where the action is
executed — a malformed value is an error, never something logged as if it had been accepted.
`sw1`/`sw2` must each be exactly one byte.

**`sw1` and `sw2` are required, and there is deliberately no default.** A tag is an
access-control device, and the only status word a default could reasonably be is `9000` —
success. That would make the most degenerate thing a model can emit, a bare
`{"type": "respond_to_apdu"}`, an approval of a VERIFY: an omission granting access, which is
the OAuth2 fail-open shape the root `CLAUDE.md` calls the most dangerous pattern here. Success
has to be *named*, the way `eapol` gives `EAP-Success` its own eight literal octets rather
than a boolean anything could flip. Both layers enforce it — `execute_action` refuses the
action, and `decode_status_byte` refuses the normalised payload — so nothing between the
model's answer and the wire can invent a status word. A `respond_to_apdu` without one is
`decision=fail_closed_no_action` and `6F00`.

## Fail closed

If the handler returns no `respond_to_apdu`, or returns one that cannot be decoded, the tag
answers **`6F00`** (ISO 7816-4 "no precise diagnosis"). If the LLM call itself *errored*, the
tag answers a status word chosen from `crate::utils::WireFailure`:

| Category | Status word | Meaning to the reader |
|----------|-------------|-----------------------|
| `WireFailure::Overloaded` | `6400` | execution error, card state unchanged — transient, retry |
| `WireFailure::Unavailable` | `6F00` | no precise diagnosis — card-side failure |

Splitting the two is the APDU equivalent of HTTP 503 vs 500: a saturated backend must not be
recorded by the reader as a permanently broken card. Nothing derived from the error reaches
the wire — a response APDU has no free-text field, and the status word is a *category*. The
error itself goes to the log and the TUI status stream only.

It never falls through to `9000` — not on an LLM error, not on silence, and not on a
`respond_to_apdu` that forgot its status word. The model's own refusal (`6982`, `6A82`,
`6D00`, …) is therefore structurally distinguishable from the model having said nothing — the
OAuth2 failure mode in the root CLAUDE.md, avoided by construction.

The five outcomes are tagged in the log so an operator can tell them apart, at WARN (the wire
*is* answered, so none of them is fatal) except the first two, which are DEBUG:

- `decision=model_answer` — the model answered with a success/warning SW (`90xx`, `61/62/63xx`)
- `decision=model_reject` — the model answered with a refusal SW of its own choosing
- `decision=fail_closed_no_action` — the handler ran and produced no `respond_to_apdu` → `6F00`
- `decision=fail_closed_undecodable` — a `respond_to_apdu` that could not be decoded → `6F00`
- `decision=fail_closed_llm_error` — the LLM call errored → `6400` / `6F00` by category

The same `decision=fail_closed_llm_error` tag is used for a failed `nfc_server_started` call,
where there is no reader to answer and the tag simply comes up with its built-in defaults.

The tag always writes *something* back, so a reader is never left hanging on its own timeout.

## Hostile input

Everything on this socket is attacker-controlled.

- The length prefix is checked against `MAX_FRAME_LEN` (4096) **before** any read, so a
  hostile length cannot make the server allocate. An oversized frame closes the connection.
- A zero-length frame is ignored.
- `ApduCommand::parse` bounds-checks every index. Short form and extended form are both
  handled; truncated, over-long and inconsistent APDUs are `Err`, never a panic. A panic in a
  connection task is silent and would leave the server reporting `Running`.
- A malformed APDU is answered `6700` locally and **does not reach the handler**, so garbage
  cannot be used to drive LLM calls.

## No connection state machine, deliberately

Other protocols hand-roll Idle → Processing → Accumulating to stop two LLM calls racing on
one connection and to reassemble partial reads. Neither problem exists here: the framing is
explicit, so there is nothing to reassemble, and the next frame is only read after the current
one has been answered, so a connection can never have two calls in flight.

## Startup parameters

- `tag_type` — `type2`, `type4` (default), `generic`. Reported to the handler in every APDU event.
- `uid` — hex; a random 7-byte UID is generated when omitted. Also reported in every APDU event.

Both are declared and both are read. Neither is interpreted by the server: nothing here
answers `FF CA 00 00` (PC/SC GET UID) on the handler's behalf.

## No built-in card logic

There is no file system, no NDEF encoder, no PIN store, no key store. The only state the
server holds is what the handler put there (`atr`, `ndef_records`) plus the two startup
parameters. Every APDU is answered by the handler — script, static, or LLM. `usb/smartcard`
took the opposite approach (a hardcoded file system, a PIN of `123456`, an RSA key); that is
storage implemented inside a protocol, which the root CLAUDE.md forbids.

## Limitations

- **No RF.** A phone tapped against a reader will never reach this. Only a reader that speaks
  vpcd over TCP can.
- **The pcscd path is untested.** `vpcd` in client mode *should* connect and work; nobody has
  run it against this server.
- Frames are capped at 4096 bytes. `ApduCommand::parse` understands the whole extended form,
  whose `Lc`/`Le` reach 65535, but a frame carrying one is refused well before that — an
  extended APDU is usable here only up to ~4089 data bytes. The cap is the bound that stops a
  hostile length prefix becoming an allocation, so it is the frame limit that is real and the
  parser's range that is theoretical.
- **`Le` is not enforced against the response.** The parser decodes it (including the
  `00`-means-256 / `0000`-means-65536 conventions) and hands it to the handler, but nothing
  truncates a body the handler made longer than the reader asked for, and nothing answers
  `6Cxx` with the correct length the way a real card would. Respecting `Le` is the handler's
  job; the event carries it for exactly that reason.
- No T=0/T=1 transmission layer, no PPS, no anti-collision, no command chaining, no
  `GET RESPONSE` bookkeeping — the handler sees whatever the reader sent and answers it.
- One handler call per APDU, so latency is one LLM round-trip per command unless a script or
  static handler is used. For anything chatty, use a script handler.
- The `nfc` Cargo feature is `nfc = []` — it pulls **nothing**. (This section previously said
  it "still pulls `pcsc` and `ndef-rs`"; neither is true, and `ndef-rs` has never been in the
  manifest at all. `pcsc` belongs to `nfc-client`, which is the only consumer. There is no
  feature to split.)

## References

- ISO/IEC 7816-4 — APDU structure and status words
- NFC Forum Type 4 Tag Operation Specification
- vsmartcard / vpcd: https://github.com/frankmorgner/vsmartcard
- PC/SC Workgroup: https://pcscworkgroup.com/
