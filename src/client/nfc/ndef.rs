//! NDEF (NFC Data Exchange Format) message encoding and decoding.
//!
//! `write_ndef` advertises a `records` array of typed records — `{"type": "text", …}`,
//! `{"type": "uri", …}` — because the root `CLAUDE.md` forbids handing a model raw bytes.
//! Something has to turn those into NDEF wire bytes, and this is it; before it existed
//! `write_ndef` accepted the records, dropped them, and reported that it needed a field
//! nothing declared.
//!
//! ## Deliberately flat: no nested messages
//!
//! NDEF is a **nesting** format — a Smart Poster record's payload is itself an NDEF message,
//! and so is a Handover Select's — which is the stack-overflow class the root `CLAUDE.md`
//! describes: a recursive decoder without a depth counter dies on a `SIGSEGV` against the
//! guard page, which is not a panic, so `catch_unwind` and `spawn_blocking` cannot contain it
//! and the whole NetGet process goes down. The bytes here come off a tag anyone can hand us.
//!
//! So the decoder **never recurses**: it walks the top-level records in a loop and returns a
//! nested message's payload as hex rather than descending into it. There is no depth counter
//! because there is no depth. The encoder mirrors that — it can build a record whose payload
//! is a caller-supplied blob, but it has no "message inside a message" record type, so a
//! model cannot ask for one either.
//!
//! ## Hostile text on both sides
//!
//! An NDEF URI record is a phishing primitive: whatever is in it is what a phone offers to
//! open. So a URI is required to be the ASCII RFC 3986 allows (no control characters, no
//! whitespace, no non-ASCII — those must be percent-encoded), and text records are refused if
//! they carry C0/C1 controls or Unicode bidirectional **override** and **isolate** characters,
//! which reverse how a string renders without changing what it is.
//!
//! Encoding refuses; decoding cannot refuse — a tag is allowed to be hostile and the model
//! needs to be told what it found — so decoding replaces those characters with U+FFFD and
//! marks the record `unsafe_characters_removed`, keeping `payload_hex` as the authoritative
//! form.
//!
//! ## References
//!
//! - NFC Forum NDEF 1.0 — record layout, TNF values, chunking
//! - NFC Forum RTD Text 1.0 — `T` record: status byte, language code, text
//! - NFC Forum RTD URI 1.0 — `U` record: one-byte prefix identifier plus the rest

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// Largest NDEF message this client will write.
///
/// A Type 4 tag publishes the message length in the two-byte `NLEN` header, so 65535 is the
/// hard ceiling of the format, not a policy choice. Range-checked *before* the `as u16` that
/// writes `NLEN`: a 70 000-byte message silently becoming `NLEN = 4464` would leave the tag
/// holding a body no reader could interpret.
pub const MAX_MESSAGE_LEN: usize = 65_535;

/// Most records one message may carry, in either direction.
///
/// A decode bound rather than a policy: the smallest possible record is three bytes, so an
/// unbounded walk over a 64 KiB file could produce ~21 000 JSON objects to put in one event.
pub const MAX_RECORDS: usize = 64;

/// Longest URI accepted in a `uri` record. Well past any real one; the point is that the
/// length is bounded before it is encoded.
const MAX_URI_LEN: usize = 4096;

// Type Name Format values (NDEF 1.0 §3.2.6).
const TNF_EMPTY: u8 = 0x00;
const TNF_WELL_KNOWN: u8 = 0x01;
const TNF_MIME: u8 = 0x02;
const TNF_ABSOLUTE_URI: u8 = 0x03;
const TNF_EXTERNAL: u8 = 0x04;
const TNF_UNKNOWN: u8 = 0x05;

// Record header flag bits (NDEF 1.0 §3.2).
const FLAG_MB: u8 = 0x80;
const FLAG_ME: u8 = 0x40;
const FLAG_CF: u8 = 0x20;
const FLAG_SR: u8 = 0x10;
const FLAG_IL: u8 = 0x08;
const MASK_TNF: u8 = 0x07;

/// The `T` (Text) and `U` (URI) well-known record types.
const RTD_TEXT: &[u8] = b"T";
const RTD_URI: &[u8] = b"U";

/// The status byte of a Text record carries the language length in its low six bits, so a
/// language code longer than this cannot be expressed. Checked before the cast, not after.
const MAX_LANGUAGE_LEN: usize = 0x3F;

/// Bit 7 of a Text record's status byte: set means the text is UTF-16, clear means UTF-8.
const TEXT_UTF16_FLAG: u8 = 0x80;

/// URI prefix identifier codes, NFC Forum RTD URI 1.0 Table 3. Index is the code byte.
const URI_PREFIXES: [&str; 36] = [
    "",
    "http://www.",
    "https://www.",
    "http://",
    "https://",
    "tel:",
    "mailto:",
    "ftp://anonymous:anonymous@",
    "ftp://ftp.",
    "ftps://",
    "sftp://",
    "smb://",
    "nfs://",
    "ftp://",
    "dav://",
    "news:",
    "telnet://",
    "imap:",
    "rtsp://",
    "urn:",
    "pop:",
    "sip:",
    "sips:",
    "tftp:",
    "btspp://",
    "btl2cap://",
    "btgoep://",
    "tcpobex://",
    "irdaobex://",
    "file://",
    "urn:epc:id:",
    "urn:epc:tag:",
    "urn:epc:pat:",
    "urn:epc:raw:",
    "urn:epc:",
    "urn:nfc:",
];

/// True for a character that must never reach a text record.
///
/// C0 controls other than tab/newline/carriage return, DEL and the C1 block, and the Unicode
/// bidirectional **overrides** and **isolates** (U+202A–U+202E, U+2066–U+2069). The plain
/// marks U+200E/U+200F are *not* here: they are ordinary in right-to-left text, while an
/// override exists to make a string render as something other than what it is.
fn is_unsafe_char(c: char) -> bool {
    matches!(c,
        '\u{0000}'..='\u{0008}'
        | '\u{000B}' | '\u{000C}'
        | '\u{000E}'..='\u{001F}'
        | '\u{007F}'..='\u{009F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}')
}

/// Refuse a model-supplied string carrying anything from [`is_unsafe_char`].
fn check_text(field: &str, value: &str) -> Result<()> {
    if let Some(c) = value.chars().find(|&c| is_unsafe_char(c)) {
        bail!(
            "'{field}' contains U+{:04X}, a control or bidirectional-override character that \
             must not go into an NDEF record: it changes how the record renders on a phone \
             without changing what it says",
            c as u32
        );
    }
    Ok(())
}

/// Replace anything [`is_unsafe_char`] rejects with U+FFFD.
///
/// The decode-side counterpart of [`check_text`]. Returns the cleaned string and whether
/// anything was removed, because the model must be told rather than quietly shown a shorter
/// string than the tag held.
fn scrub_text(value: &str) -> (String, bool) {
    let mut removed = false;
    let cleaned = value
        .chars()
        .map(|c| {
            if is_unsafe_char(c) {
                removed = true;
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    (cleaned, removed)
}

/// Encode a message's worth of typed records into NDEF wire bytes.
///
/// Fails rather than truncating, everywhere: an over-long message, an over-long language
/// code, a URI that is not the ASCII RFC 3986 permits, an unknown record type. A tag written
/// with a body that does not match its own `NLEN` is worse than a tag not written at all.
pub fn encode_message(records: &[Value]) -> Result<Vec<u8>> {
    if records.is_empty() {
        bail!("an NDEF message needs at least one record");
    }
    if records.len() > MAX_RECORDS {
        bail!(
            "an NDEF message may carry at most {} records, got {}",
            MAX_RECORDS,
            records.len()
        );
    }

    let mut out = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let (tnf, type_field, payload) = encode_one(record)
            .map_err(|e| anyhow!("NDEF record {} ({}): {e}", i + 1, describe(record)))?;
        push_record(
            &mut out,
            tnf,
            &type_field,
            &payload,
            i == 0,
            i + 1 == records.len(),
        );
        if out.len() > MAX_MESSAGE_LEN {
            bail!(
                "NDEF message exceeds the {} bytes a Type 4 tag's two-byte NLEN can describe",
                MAX_MESSAGE_LEN
            );
        }
    }
    Ok(out)
}

/// A short description of a record for an error message, without dumping its content.
fn describe(record: &Value) -> String {
    record
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("no 'type'")
        .to_string()
}

/// Build one record's TNF, TYPE field and payload from its JSON form.
fn encode_one(record: &Value) -> Result<(u8, Vec<u8>, Vec<u8>)> {
    let kind = record
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("a record needs a 'type' ('text', 'uri', 'mime' or 'external')"))?;

    match kind {
        "text" => {
            let text = record
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("a text record needs 'text'"))?;
            check_text("text", text)?;

            let language = record
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("en");
            if language.is_empty() || language.len() > MAX_LANGUAGE_LEN {
                bail!(
                    "'language' must be 1..={} bytes (the Text status byte carries its length \
                     in six bits), got {}",
                    MAX_LANGUAGE_LEN,
                    language.len()
                );
            }
            if !language
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                bail!("'language' must be an IANA language tag such as 'en' or 'en-GB'");
            }

            // Range-checked above, so the cast cannot lose a bit; the status byte is
            // `UTF-16 flag | RFU | six bits of length`, and we always write UTF-8.
            let mut payload = vec![language.len() as u8];
            payload.extend_from_slice(language.as_bytes());
            payload.extend_from_slice(text.as_bytes());
            Ok((TNF_WELL_KNOWN, RTD_TEXT.to_vec(), payload))
        }
        "uri" => {
            let uri = record
                .get("uri")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("a uri record needs 'uri'"))?;
            check_uri(uri)?;

            // Longest matching prefix wins, so "https://www.x" abbreviates to code 2 rather
            // than code 4 plus a literal "www.".
            let (code, rest) = URI_PREFIXES
                .iter()
                .enumerate()
                .skip(1)
                .filter(|(_, prefix)| uri.starts_with(*prefix))
                .max_by_key(|(_, prefix)| prefix.len())
                .map(|(code, prefix)| (code as u8, &uri[prefix.len()..]))
                .unwrap_or((0, uri));

            let mut payload = vec![code];
            payload.extend_from_slice(rest.as_bytes());
            Ok((TNF_WELL_KNOWN, RTD_URI.to_vec(), payload))
        }
        "mime" => {
            let mime_type = record
                .get("mime_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("a mime record needs 'mime_type', e.g. 'text/plain'"))?;
            check_text("mime_type", mime_type)?;
            if !mime_type.is_ascii() || mime_type.is_empty() {
                bail!("'mime_type' must be a non-empty ASCII media type");
            }
            Ok((TNF_MIME, mime_type.as_bytes().to_vec(), payload_of(record)?))
        }
        "external" => {
            let domain_type = record
                .get("domain_type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow!("an external record needs 'domain_type', e.g. 'example.com:mytype'")
                })?;
            check_text("domain_type", domain_type)?;
            if !domain_type.contains(':') {
                bail!("'domain_type' must be 'domain:type', e.g. 'example.com:mytype'");
            }
            Ok((
                TNF_EXTERNAL,
                domain_type.as_bytes().to_vec(),
                payload_of(record)?,
            ))
        }
        other => bail!(
            "unknown NDEF record type '{other}'; this client writes 'text', 'uri', 'mime' and \
             'external'. Nested messages (smart poster, handover) are deliberately not \
             supported - see the module docs"
        ),
    }
}

/// The payload of a `mime` or `external` record: text or hex, never both.
///
/// The same rule as the server's `respond_to_apdu` and `send_tcp_data`: "48656C6C6F" is
/// simultaneously valid text and valid hex and only the sender knows which it meant, so the
/// encoding is declared rather than sniffed.
fn payload_of(record: &Value) -> Result<Vec<u8>> {
    let text = record.get("payload_text").and_then(|v| v.as_str());
    let hex_str = record.get("payload_hex").and_then(|v| v.as_str());
    match (text, hex_str) {
        (Some(text), None) => {
            check_text("payload_text", text)?;
            Ok(text.as_bytes().to_vec())
        }
        (None, Some(hex_str)) => hex::decode(hex_str.trim())
            .map_err(|e| anyhow!("'payload_hex' is not valid hexadecimal: {e}")),
        (None, None) => Ok(Vec::new()),
        (Some(_), Some(_)) => bail!("a record takes 'payload_text' or 'payload_hex', not both"),
    }
}

/// Reject a URI that is not what RFC 3986 allows on the wire.
///
/// A URI is US-ASCII with no whitespace; everything else is percent-encoded. Requiring that
/// is what makes a control character, a newline or a bidirectional override impossible here
/// rather than merely discouraged — none of them is in `0x21..=0x7E`.
fn check_uri(uri: &str) -> Result<()> {
    if uri.is_empty() {
        bail!("'uri' must not be empty");
    }
    if uri.len() > MAX_URI_LEN {
        bail!("'uri' is {} bytes, over the {MAX_URI_LEN} limit", uri.len());
    }
    if let Some(b) = uri.bytes().find(|b| !(0x21..=0x7E).contains(b)) {
        bail!(
            "'uri' contains byte 0x{b:02X}, which RFC 3986 does not allow unencoded: a URI is \
             printable US-ASCII with no whitespace, and percent-encoding is how anything else \
             travels. An NDEF URI record is what a phone offers to open, so this is refused \
             rather than escaped"
        );
    }
    Ok(())
}

/// Append one encoded record, choosing the short or long payload-length form.
fn push_record(
    out: &mut Vec<u8>,
    tnf: u8,
    type_field: &[u8],
    payload: &[u8],
    first: bool,
    last: bool,
) {
    let short = payload.len() <= u8::MAX as usize;
    let mut header = tnf & MASK_TNF;
    if first {
        header |= FLAG_MB;
    }
    if last {
        header |= FLAG_ME;
    }
    if short {
        header |= FLAG_SR;
    }
    out.push(header);
    // A TYPE field is at most 255 bytes by the format; every type this encoder produces is a
    // one-byte RTD, a media type or a domain:type, all far shorter, and `encode_one` is the
    // only caller.
    out.push(type_field.len().min(u8::MAX as usize) as u8);
    if short {
        out.push(payload.len() as u8);
    } else {
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    }
    out.extend_from_slice(type_field);
    out.extend_from_slice(payload);
}

/// Walk an NDEF message into typed JSON records.
///
/// **Iterative, and never descends into a payload** — see the module docs. A payload that is
/// itself an NDEF message comes back as hex, which is the whole reason this cannot be
/// stack-overflowed by a tag.
///
/// Returns what it managed to read: a truncated or malformed tail becomes a final
/// `{"type": "undecodable", …}` record rather than an error, because a partly-readable tag is
/// still worth telling the model about.
pub fn decode_message(bytes: &[u8]) -> Result<Vec<Value>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let mut at = 0usize;

    while at < bytes.len() {
        if records.len() >= MAX_RECORDS {
            records.push(json!({
                "type": "undecodable",
                "reason": format!("stopped after {MAX_RECORDS} records"),
                "remaining_bytes": bytes.len() - at,
            }));
            break;
        }
        match decode_one(bytes, at) {
            Ok((record, next)) => {
                let last = record
                    .get("message_end")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                records.push(record);
                at = next;
                if last {
                    break;
                }
            }
            Err(e) => {
                records.push(json!({
                    "type": "undecodable",
                    "reason": e.to_string(),
                    "offset": at,
                    "remaining_hex": hex::encode_upper(&bytes[at..]),
                }));
                break;
            }
        }
    }

    Ok(records)
}

/// Decode the record starting at `at`, returning it and the offset of the next one.
fn decode_one(bytes: &[u8], at: usize) -> Result<(Value, usize)> {
    let header = *bytes
        .get(at)
        .ok_or_else(|| anyhow!("record header past the end of the message"))?;
    let tnf = header & MASK_TNF;
    let short = header & FLAG_SR != 0;
    let has_id = header & FLAG_IL != 0;
    let chunked = header & FLAG_CF != 0;
    let message_end = header & FLAG_ME != 0;

    let mut at = at + 1;
    let type_len = *take(bytes, at, 1)?.first().unwrap() as usize;
    at += 1;

    // Widening casts only: a u8 or a u32 read from the wire becomes a usize, which cannot
    // lose a bit. The bounds check below is what keeps the value honest.
    let payload_len = if short {
        at += 1;
        *take(bytes, at - 1, 1)?.first().unwrap() as usize
    } else {
        let raw = take(bytes, at, 4)?;
        at += 4;
        u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize
    };

    let id_len = if has_id {
        at += 1;
        *take(bytes, at - 1, 1)?.first().unwrap() as usize
    } else {
        0
    };

    let type_field = take(bytes, at, type_len)?.to_vec();
    at += type_len;
    let id_field = take(bytes, at, id_len)?.to_vec();
    at += id_len;
    let payload = take(bytes, at, payload_len)?.to_vec();
    at += payload_len;

    let mut record = decode_payload(tnf, &type_field, &payload, chunked);
    if let Some(map) = record.as_object_mut() {
        map.insert("message_end".into(), json!(message_end));
        if chunked {
            map.insert("chunked".into(), json!(true));
        }
        if has_id {
            map.insert("record_id_hex".into(), json!(hex::encode_upper(&id_field)));
        }
    }
    Ok((record, at))
}

/// Bounds-checked slice. Every field of every record goes through this, so a length the tag
/// made up is an `Err` rather than a panic in a connection task.
fn take(bytes: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    bytes.get(at..at.saturating_add(len)).ok_or_else(|| {
        anyhow!(
            "record claims {len} bytes at offset {at}, past the {} it has",
            bytes.len()
        )
    })
}

/// Turn one decoded record's TNF, TYPE and payload into JSON.
///
/// A **chunked** record is reported rather than reassembled: the payload of the first chunk
/// alone is not the record's content, so decoding it as text would show the model a fragment
/// as though it were whole.
fn decode_payload(tnf: u8, type_field: &[u8], payload: &[u8], chunked: bool) -> Value {
    let payload_hex = hex::encode_upper(payload);

    if chunked {
        return json!({
            "type": "chunked",
            "note": "this record is a chunk; NetGet does not reassemble chunked records, so \
                     the payload below is a fragment",
            "tnf": tnf,
            "record_type_hex": hex::encode_upper(type_field),
            "payload_hex": payload_hex,
        });
    }

    match (tnf, type_field) {
        (TNF_WELL_KNOWN, RTD_TEXT) => match decode_text(payload) {
            Some((language, text, encoding)) => {
                let (text, removed) = scrub_text(&text);
                let mut value = json!({
                    "type": "text",
                    "language": language,
                    "text": text,
                    "text_encoding": encoding,
                    "payload_hex": payload_hex,
                });
                if removed {
                    value["unsafe_characters_removed"] = json!(true);
                }
                value
            }
            None => json!({
                "type": "undecodable",
                "reason": "a Text record whose status byte does not match its payload length",
                "payload_hex": payload_hex,
            }),
        },
        (TNF_WELL_KNOWN, RTD_URI) => match decode_uri(payload) {
            Some(uri) => {
                let (uri, removed) = scrub_text(&uri);
                let mut value = json!({
                    "type": "uri",
                    "uri": uri,
                    "payload_hex": payload_hex,
                });
                if removed {
                    value["unsafe_characters_removed"] = json!(true);
                }
                value
            }
            None => json!({
                "type": "undecodable",
                "reason": "an empty URI record",
                "payload_hex": payload_hex,
            }),
        },
        (TNF_MIME, _) => json!({
            "type": "mime",
            "mime_type": String::from_utf8_lossy(type_field),
            "payload_hex": payload_hex,
            "payload_text": printable(payload),
        }),
        (TNF_ABSOLUTE_URI, _) => {
            let (uri, removed) = scrub_text(&String::from_utf8_lossy(type_field));
            json!({
                "type": "absolute_uri",
                "uri": uri,
                "unsafe_characters_removed": removed,
                "payload_hex": payload_hex,
            })
        }
        (TNF_EXTERNAL, _) => json!({
            "type": "external",
            "domain_type": String::from_utf8_lossy(type_field),
            "payload_hex": payload_hex,
            "payload_text": printable(payload),
        }),
        (TNF_EMPTY, _) => json!({ "type": "empty" }),
        (TNF_UNKNOWN, _) => json!({
            "type": "unknown",
            "payload_hex": payload_hex,
        }),
        _ => json!({
            "type": "other",
            "tnf": tnf,
            "record_type_hex": hex::encode_upper(type_field),
            "payload_hex": payload_hex,
        }),
    }
}

/// Split a Text record's payload into (language, text, encoding name).
fn decode_text(payload: &[u8]) -> Option<(String, String, &'static str)> {
    let status = *payload.first()?;
    let lang_len = (status & 0x3F) as usize;
    let language = payload.get(1..1 + lang_len)?;
    let text = payload.get(1 + lang_len..)?;

    if status & TEXT_UTF16_FLAG == 0 {
        return Some((
            String::from_utf8_lossy(language).to_string(),
            String::from_utf8_lossy(text).to_string(),
            "utf-8",
        ));
    }

    // UTF-16, big-endian unless a BOM says otherwise (RTD Text 1.0 §3.2.1). A trailing odd
    // byte is dropped rather than being an error: a tag is allowed to be wrong, and half a
    // code unit is nothing the model needs to see.
    let (rest, little_endian, encoding): (&[u8], bool, &'static str) = match text {
        [0xFF, 0xFE, rest @ ..] => (rest, true, "utf-16le"),
        [0xFE, 0xFF, rest @ ..] => (rest, false, "utf-16be"),
        rest => (rest, false, "utf-16be"),
    };
    let units: Vec<u16> = rest
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if little_endian {
                u16::from_le_bytes(*c)
            } else {
                u16::from_be_bytes(*c)
            }
        })
        .collect();
    Some((
        String::from_utf8_lossy(language).to_string(),
        String::from_utf16_lossy(&units),
        encoding,
    ))
}

/// Expand a URI record's payload: one prefix-identifier byte plus the rest.
fn decode_uri(payload: &[u8]) -> Option<String> {
    let code = *payload.first()? as usize;
    let prefix = URI_PREFIXES.get(code).copied().unwrap_or("");
    Some(format!(
        "{prefix}{}",
        String::from_utf8_lossy(payload.get(1..).unwrap_or_default())
    ))
}

/// A text view of an opaque payload, but only when every byte is printable ASCII — the same
/// rule the NFC server uses for an APDU data field.
fn printable(payload: &[u8]) -> Value {
    if !payload.is_empty()
        && payload
            .iter()
            .all(|&b| b.is_ascii_graphic() || b == b' ' || b == b'\t')
    {
        json!(String::from_utf8_lossy(payload))
    } else {
        Value::Null
    }
}
