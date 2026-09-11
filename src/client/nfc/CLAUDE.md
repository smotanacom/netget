# NFC (Near Field Communication) Client

## What this is

A **PC/SC reader client**: it drives a physical contactless reader, sends ISO 7816-4 APDUs to
whatever card is in the field, and reads and writes NFC Forum Type 4 NDEF messages. The model
decides what to send and what to make of the answer.

The NFC **server** is a different thing entirely and shares no code: it *is* a virtual tag, on
a TCP socket, needing no hardware. See `src/server/nfc/CLAUDE.md`. This file compiles under the
**`nfc-client`** feature, not `nfc` — several `cargo check --features nfc` runs have reported
success without compiling a line of it.

`DevelopmentState` is `Experimental`. It was `Incomplete` until September 2026, which means
`is_available_to_llm()` returned false and **the model could not see this client at all** —
a leftover from when the NDEF verbs did nothing, and a contradiction of the root `CLAUDE.md`'s
claim that no `Incomplete` protocol remained. Hiding a protocol is not how an unfinished one is
reported: the `bluetooth_ble_beacon` precedent is to expose it and say plainly what is untested.

## Library

**pcsc 2.9** — PC/SC bindings. Native WinSCard on Windows, native PCSC framework on macOS,
pcsclite + `pcscd` on Linux (`apt install pcscd libpcsclite-dev`).

That is the only dependency the feature adds. **`ndef-rs` is not used and has never been in the
manifest** — this file claimed it for a long time. NDEF encoding and decoding are ours, in
`ndef.rs`, which is why the record-level behaviour can be pinned against literal specification
bytes with no reader attached.

Hardware, when someone wants to validate it: an ACR122U (~$40) or any PC/SC reader with
ISO 14443 support, plus an NTAG213 or MIFARE Ultralight (~$1).

## Architecture

`connect()` establishes a PC/SC context, lists readers, picks one by `reader_name` or
`reader_index`, and then starts **three** things:

1. **The command channel** (`command_loop`) — the dashboard's `[ send ]`. Registered *before*
   the readers-listed LLM call, because a dashboard-created client defaults to a `*` → manual
   rule and that call can park for minutes waiting for a human; the operator must be able to
   drive the reader while it waits. The generic `handle_stream_client_command` cannot serve
   this client — it owns no socket — so injected actions go through `apply_nfc_action`, the
   same function every other path uses.
2. **The card watcher** — polls `SCardConnect` every 500 ms on the blocking pool and raises
   `nfc_card_detected` / `nfc_card_disconnected` on the edges. It reads the ATR *and* the
   negotiated protocol (`status2_owned`), because `protocol` is a declared parameter of that
   event and used to be declared and never emitted.
3. **The readers-listed LLM call** — one turn to let the model react to what is attached. Its
   failure is logged, never fatal: the command channel is already up.

All three are registered with `register_client_task`, so removing the client stops them.

**No card handle is kept between commands.** Every APDU is `SCardConnect` + `SCardTransmit` +
drop, on a `spawn_blocking` thread (PC/SC is a blocking C API). A card removed and re-presented
between two commands therefore still works, at the cost of a connect per command.

## The model's vocabulary

| Action | Reaches the card | Notes |
|---|---|---|
| `send_apdu` | yes | structured: `cla`, `ins`, `p1`, `p2`, optional `data` and `le` |
| `send_apdu_raw` | yes | one hex string, for a command copied from a datasheet |
| `read_ndef` | yes | Type 4 sequence; raises `nfc_ndef_read` with decoded records |
| `write_ndef` | yes | typed `records`, encoded here into NDEF bytes |
| `disconnect_card` | — | ends the session and drops the command handle |
| `wait_for_more` | — | "that answer was partial" |

There is deliberately **no** `connect_card` or `list_readers`. Both were once advertised with
no arm in `execute_action`, so both came back "Unknown action type" and cost the model a retry.
They are worth adding, but they need PC/SC work in `mod.rs` first, not a declaration on its own.

`get_sync_actions()` carries the protocol verbs and `get_async_actions()` carries only
`disconnect_card`. **Do not "tidy" that into one list.** `client_llm_action_set` unions async ∪
sync ∪ the firing event's actions, so the split is invisible to the model either way — but
`events::handler` builds a **static handler's** catalogue from `get_sync_actions()` plus event
actions, never the async list. A verb moved to async-only cannot be named by a static handler,
and the client's own static-mode startup example (`read_ndef`) would stop being creatable.
`src/cli/rolling_tui.rs::execute_single_task` reads the sync list too.

### APDU fields are range-checked before they become bytes

`send_apdu` used to interpolate the model's strings into a hex string and derive `Lc` as
`data.len() / 2`. Three ways that put a command on the card that the model did not choose:

- `"p1": "4"` instead of `"04"` — one digit, and every byte after it shifts by four bits.
- an odd-length `data` — the length floors, and the data field is misaligned.
- more than 255 data bytes — `format!("{:02X}", 256)` prints `100`, **three** digits into a
  two-digit field. `{:02X}` pads; it does not truncate.

None produces a *malformed* APDU. Each produces a different, valid-looking one — which against
a card is the difference between a SELECT and something else entirely. Every field is now
decoded and length-checked first, over 255 data bytes is refused with a pointer to
`send_apdu_raw`, and `send_apdu_raw` itself must be valid hex of at least the four-byte header.

## NDEF (`ndef.rs`)

`write_ndef` advertises typed records because the root `CLAUDE.md` forbids handing a model raw
bytes. Something has to turn those into wire bytes, and until September 2026 nothing did:
`execute_action` produced `{"records": [...]}` while `mod.rs` looked for `message_hex` or
`message`, which nothing declared and nothing produced. **Every `write_ndef` returned "needs
message_hex or message" and the verb could not write a single byte** — advertised, accepted,
inert.

Encoding supports `text` (RTD Text, with `language`), `uri` (RTD URI, with the prefix table and
longest-match abbreviation), `mime` and `external`. Decoding handles those plus absolute URI,
empty and unknown TNFs, and reports anything else with its raw bytes.

**Nested messages are deliberately not supported, in either direction.** NDEF nests — a Smart
Poster's payload is itself an NDEF message — and that is the stack-overflow class the root
`CLAUDE.md` describes: a recursive decoder without a counter dies on a `SIGSEGV` against the
guard page, which is not a panic, so `catch_unwind` and `spawn_blocking` cannot contain it and
the whole NetGet process goes down. The bytes come off a tag anyone can hand us. So the decoder
walks the top level in a loop and returns a nested payload as hex; there is no depth counter
because there is no depth. The encoder has no nesting record type, so a model cannot ask for
one either.

### Hostile text, both directions

An NDEF URI record is a **phishing primitive**: whatever is in it is what a phone offers to
open. So:

- **Encoding refuses.** A URI must be printable US-ASCII with no whitespace, which is what
  RFC 3986 allows unencoded — that makes a newline, a NUL and a bidirectional override
  impossible rather than merely discouraged. Text records are refused if they carry C0/C1
  controls or the Unicode bidi **overrides**/**isolates** (U+202A–U+202E, U+2066–U+2069);
  U+202E renders `gpj.exe` as `exe.jpg`. The plain marks U+200E/U+200F are allowed — they are
  ordinary in right-to-left text.
- **Decoding cannot refuse.** A tag is allowed to be hostile and the model has to be told what
  was there. So those characters are replaced with U+FFFD, the record is flagged
  `unsafe_characters_removed`, and `payload_hex` stays as the authoritative form.

### The Type 4 read/write sequence

SELECT the NDEF application by AID `D2760000850101`, SELECT the NDEF file by `file_id`, then
READ BINARY (two-byte `NLEN` first, then the body) or UPDATE BINARY. Writing zeroes `NLEN`
first and publishes the real length last, as the specification requires: a reader interrupting
the write mid-way then sees an empty message rather than a truncated one it would parse as
valid.

**It does not read the Capability Container.** `file_id` defaults to `E104` — what nearly every
tag's CC points at — and the CC's `MLe`/`MLc` limits are ignored. Reading the CC is the obvious
next improvement and would let `file_id` be dropped. `file_id` is declared on both verbs; it
used to be read in `mod.rs` and dropped by `execute_action`, so the parameter did nothing.

Two arithmetic defects that the sequence had, both of which the specification allows a real tag
to trigger:

- **`Le` is a maximum, not a promise.** The read loop advanced its offset by the number of
  bytes *requested*, so a tag answering fewer skipped everything it withheld — and a tag
  answering zero data bytes with `9000` made the loop run forever, hammering the card. It now
  advances by what came back and bails when a read makes no progress.
- **`(offset >> 8) as u8` wraps past 65535**, and a wrapped offset is not a failed command: it
  is a *successful* read or write of the wrong part of the file. A message longer than 65535
  additionally wrote `NLEN = len as u16`, so a 70 000-byte message would have published 4464
  and left the tag describing itself wrongly. Both are range-checked before the cast now.

## Events

| Event | Raised when | Carries |
|---|---|---|
| `nfc_readers_listed` | once, after enumeration | `readers` |
| `nfc_card_detected` | a card enters the field | `atr`, `protocol` |
| `nfc_card_disconnected` | it leaves | — |
| `nfc_apdu_response` | a card answered an APDU | `response_hex`, `sw1`, `sw2`, `data_hex` |
| `nfc_ndef_read` | `read_ndef` succeeded | `records`, `length`, `message_hex`, `message_text` |

All five are emitted. `nfc_ndef_read`'s `records` was declared and never emitted until the
codec existed — the model was promised typed records and handed only hex.

## The model's answer is executed, bounded

All three event paths were once `if let Err(e) = call_llm_for_client(..)` with no success arm,
so every action the model chose in reply was dropped. **`nfc_apdu_response` was the one that
mattered**: a smartcard exchange is inherently a conversation — SELECT an application, then
READ BINARY against what SELECT returned — and the model was told what the card said and then
never asked again, so nothing beyond a single injected APDU was reachable.

The stated reason for the card-presence path was that "a presence-driven action chain would
re-fire every time a card is tapped". The re-fire concern is real; silence is the wrong remedy.
A tap is a discrete event, so one **bounded** chain per tap is the right shape:
`MAX_FOLLOWUP_DEPTH = 6`, with the limit reported on the status stream when it is hit.

`apply_nfc_action` returns an explicitly boxed `Pin<Box<dyn Future + Send>>` rather than being
an `async fn`, because `apply_nfc_action` → `notify_apdu_response` → `run_followups` →
`apply_nfc_action` is a cycle of `async fn`s whose opaque return types cannot be inferred
(E0391).

## Command channel outcomes

| action | `ClientSendOutcome` |
|---|---|
| `send_apdu` / `send_apdu_raw`, card answered | `Sent { bytes_sent }` (response hex in the access log) |
| `send_apdu` with no card in the field | `Executed { detail: "send_apdu did not reach a card: …" }` |
| `read_ndef` / `write_ndef` | `Executed { detail }` naming what happened, or `Sent` for a completed write |
| `disconnect_card` | `Disconnected` (handle dropped) |
| unknown verb | `Rejected { error }` |

`Sent` is reported **only** when bytes really crossed the contactless interface. Everything
else says specifically why nothing did — the distinction is the point.

## Startup parameters

- `reader_index` — 0-based, default 0.
- `reader_name` — substring match, overrides `reader_index`.

Both declared, both read. The reader is kept as a `CString` throughout rather than round-tripped
through `String`, which can lose a non-UTF-8 name.

## Limitations

- **PC/SC only.** APDU-based cards: ISO 14443 A/B, MIFARE, NFC Forum Type 2/4. No libnfc, so no
  raw NFC commands and no peer-to-peer (NFC-DEP).
- **Reader mode only.** Card emulation needs hardware PC/SC does not expose; NetGet's *server*
  covers that case over a socket instead.
- **The card path is unverified.** This machine has no reader, and a contactless card cannot be
  emulated through PC/SC — `SCardConnect` needs one in the field. The APDU byte sequences come
  from the specification, not from observation. `metadata()` says so.
- Type 4 only for the NDEF verbs; a Type 2 tag (NTAG213 and friends) answers different
  commands. `send_apdu` still reaches it.
- MIFARE Classic sector authentication is not implemented.
- No `GET RESPONSE` chaining: a card answering `61xx` is reported to the model, which may send
  the follow-up itself.

## References

- PC/SC Workgroup: https://pcscworkgroup.com/
- ISO/IEC 7816-4 — APDU structure and status words
- NFC Forum Type 4 Tag Operation — the SELECT/READ BINARY sequence
- NFC Forum NDEF 1.0, RTD Text 1.0, RTD URI 1.0 — `ndef.rs` implements these
