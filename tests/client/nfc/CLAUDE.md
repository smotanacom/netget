# NFC Client Testing

## What can and cannot be tested here

The NFC client is a **PC/SC reader** client. Two facts decide everything below:

- `SCardListReaders` returning nothing makes `connect()` fail, so on a machine with no reader
  the client cannot be started at all.
- **A contactless card cannot be emulated through PC/SC.** `SCardConnect` needs a card in the
  reader's field, and there is no software stand-in — unlike the *server*, which is a virtual
  tag on a TCP socket and needs no hardware whatsoever (`tests/server/nfc/`).

So the card path has no automated evidence, and saying otherwise would be a fabricated pass.
What it does have is everything on *this* side of the reader, and that is where the defects
were.

## The split

| File | Needs hardware | What it holds |
|---|---|---|
| `e2e_test.rs` | **no** | the NDEF codec and the APDU builder, against literal specification bytes |
| `command_channel_test.rs` | partly | the dashboard's `[ send ]` path; the no-reader half always runs |

`e2e_test.rs` used to be a single `#[ignore]`d `fn test_nfc_client_basic()` whose body was a
TODO comment. It asserted nothing and needed hardware it could never have — an `#[ignore]`d
test is not evidence, which the root `CLAUDE.md` says in as many words. It now holds 22 tests
that need nothing but the crate.

**The insight worth reusing: the parts most likely to be wrong were the pure ones.** NDEF
encoding, NDEF decoding and APDU construction are functions from JSON to bytes. None of them
needs a reader, and all three were broken — `write_ndef` could not encode anything at all, and
the APDU builder produced *shifted but valid* commands from a one-digit `p1`. "This protocol
needs hardware" had been read as "this protocol cannot be tested", and it was never true.

## What `e2e_test.rs` pins

Against the specifications, not against our own output:

- **RTD Text 1.0** — `D1 01 0D 54 02 'e' 'n' "Hello NFC!"`: status byte carrying the language
  length in six bits, then the language, then UTF-8.
- **RTD URI 1.0** — `D1 01 0C 55 04 "example.com"`: the one-byte prefix identifier, and that
  the *longest* matching prefix wins (`https://www.` is code 2, not code 4 plus a literal
  `www.`).
- **NDEF 1.0 framing** — MB on the first record and ME on the last, and that a payload over
  255 bytes switches to the four-byte length form with SR clear.
- **Refusals**: a URI outside printable US-ASCII (RFC 3986), text carrying a bidirectional
  override (U+202E renders `gpj.exe` as `exe.jpg`), a language code too long for six bits,
  both payload spellings at once.
- **Decoding**: round trip; a record claiming more bytes than it has is an `undecodable`
  record rather than an index panic; a hostile URI from a tag is scrubbed, flagged
  `unsafe_characters_removed`, and its raw bytes kept.
- **No recursion** — `decoding_never_descends_into_a_nested_message` wraps a record in
  message-inside-a-message until the nesting is thousands deep and asserts the payload comes
  back as *hex*. NDEF nests, and a recursive decoder without a counter is the stack-overflow
  class: a `SIGSEGV` against the guard page, which is not a panic, so nothing catches it and
  the whole NetGet process dies. The guard here is structural rather than a depth counter —
  there is no depth because the decoder never descends — and this test is what keeps it that
  way.
- **The APDU builder**: the declared SELECT-NDEF example builds
  `00A4040007D276000085010100`; an odd-length `data`, a `p1` that is not exactly one byte, and
  more than 255 data bytes are each refused.

## LLM call budget

**Zero.** Nothing in this directory calls a model: `e2e_test.rs` is pure functions, and
`command_channel_test.rs` drives `AppState::send_to_client`, which executes an action inside
the client's own loop without asking anything. The client is pointed at
`http://127.0.0.1:1`, which nothing listens on, so a call would fail loudly rather than
quietly succeed.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features nfc-client \
    --test client -- nfc --test-threads=100
```

Note `--test client` names the test *target* and everything after `--` is a filter;
`--test client::nfc::e2e_test` is not a valid invocation and makes cargo list targets and
exit, which reads like a broken suite.

## The hardware half

`injected_apdu_reaches_a_real_card` is `#[ignore]`d and needs a PC/SC reader with a card
presented. It is the only test that can report `Sent`, because that outcome means bytes
actually crossed the contactless interface. **It is not evidence for the maturity rating** and
the client's `metadata()` says so.

What would change the rating: an ACR122U (~$40) and an NTAG213 or MIFARE Ultralight (~$1),
with the test converted to hard-fail when the reader is absent — the shape `npm`'s real-CLI
test uses — so that a machine without one fails loudly rather than skipping. That would make
the reader a requirement wherever the suite runs, which is why it has not been done
unilaterally.
