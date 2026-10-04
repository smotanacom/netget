# HL7 v2 MLLP endpoint — Experimental

Receives HL7 v2 ER7 messages over MLLP (`<VT> message <FS><CR>`, TCP 2575) and acknowledges
each one as the handler decides. No clinical store, no message-profile conformance engine.

`wire.rs` (shared with the client):
- One frame at a time: the first byte must be `<VT>`; the length is bounded (1 MiB) as it
  grows; `<FS>` must be followed by `<CR>`; a stray `<VT>`/`<FS>` or bytes after the end block
  refuse the frame (original-mode: one message in flight). Idle `idle_timeout_secs` (600)
  between messages, 30 s inside a frame.
- Parsing: MSH first, standard `|` separator and `^~\&` encoding characters, MSH-9 and MSH-10
  present, segment ids three upper-case letters/digits, ≤4096 segments, ≤512 fields, no
  control characters. Bytes that are not UTF-8 are read as ISO-8859-1 and flagged
  `charset_assumed`. `fields[n]` is field n+1 (MSH: `fields[0]` = MSH-2, so MSH-9 =
  `fields[7]`).
- Building: Rust writes MSH (timestamp, control id) and MSA; a `|` in a handler's field is
  escaped `\F\`; CR/LF/control characters are refused (they would forge a segment).

`hl7_ack` {code AA|AE|AR|CA|CE|CR, text, error {code, severity, message, location} → ERR,
segments (e.g. query responses)}. The ACK swaps sender and receiver, is typed
`ACK^<trigger>^ACK`, carries a fresh `NG<n>` control id and echoes MSA-2. A handler failure or
invalid answer is **AE** with ERR `207^Application internal error^HL70357` — never AA; the log
carries the `decision=` tag. A message with no parseable MSH cannot be acknowledged: the
connection closes.
