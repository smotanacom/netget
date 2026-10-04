//! MLLP framing (`<VT> message <FS><CR>`) and HL7 v2 ER7 segments, shared by both roles.
//!
//! Messages are exposed as structured segments: `{id, fields}` where `fields[0]` is the first
//! field after the segment id (for MSH, `fields[0]` is MSH-2, the encoding characters, so
//! `fields[n]` is always field n+1 — MSH-9 is `fields[7]`). Field text keeps the message's own
//! component (`^`), repetition (`~`), sub-component (`&`) and escape (`\`) characters; the
//! field separator and segment terminator can never appear inside a field.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const START: u8 = 0x0b;
pub const END: [u8; 2] = [0x1c, 0x0d];
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_SEGMENTS: usize = 4096;
pub const MAX_FIELDS: usize = 512;
pub const MAX_FIELD_BYTES: usize = 64 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub id: String,
    #[serde(default)]
    pub fields: Vec<String>,
}

fn segment_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 3
            && id
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            && id.as_bytes()[0].is_ascii_uppercase(),
        "segment id '{id}' must be three upper-case letters or digits"
    );
    Ok(())
}

/// A field the model supplies: structure characters stay, the field separator is escaped
/// (`\F\`), and a CR, LF or other control character — which would end the segment and forge
/// the next one — is refused.
pub fn field(text: &str) -> Result<String> {
    ensure!(
        text.len() <= MAX_FIELD_BYTES,
        "HL7 field exceeds {MAX_FIELD_BYTES} bytes"
    );
    ensure!(
        !text.chars().any(|c| c.is_control()),
        "HL7 field contains a control character (CR would start a forged segment)"
    );
    Ok(text.replace('|', "\\F\\"))
}

/// A parsed message.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub segments: Vec<Segment>,
    /// true when the bytes were not UTF-8 and were read as ISO-8859-1 (every byte maps).
    pub latin1: bool,
}

impl Message {
    pub fn msh(&self, n: usize) -> &str {
        // MSH-n is fields[n-2] (MSH-1 is the separator itself).
        self.segments
            .first()
            .and_then(|s| s.fields.get(n.wrapping_sub(2)))
            .map(String::as_str)
            .unwrap_or("")
    }
    pub fn control_id(&self) -> &str {
        self.msh(10)
    }
    pub fn message_type(&self) -> &str {
        self.msh(9)
    }
    pub fn to_event(&self) -> Value {
        json!({
            "message_type": self.message_type(),
            "control_id": self.control_id(),
            "version": self.msh(12),
            "processing_id": self.msh(11),
            "sending_application": self.msh(3),
            "sending_facility": self.msh(4),
            "receiving_application": self.msh(5),
            "receiving_facility": self.msh(6),
            "segments": self.segments,
            "charset_assumed": if self.latin1 { Value::from("ISO-8859-1") } else { Value::Null },
        })
    }
}

/// Parse one ER7 message (the bytes between the MLLP start and end blocks).
pub fn parse(bytes: &[u8]) -> Result<Message> {
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "HL7 message exceeds {MAX_MESSAGE_BYTES} bytes"
    );
    let (text, latin1) = match std::str::from_utf8(bytes) {
        Ok(t) => (t.to_owned(), false),
        Err(_) => (bytes.iter().map(|&b| b as char).collect::<String>(), true),
    };
    ensure!(text.starts_with("MSH"), "HL7 message must start with MSH");
    let separator = text
        .chars()
        .nth(3)
        .context("MSH without a field separator")?;
    ensure!(
        separator == '|',
        "only the standard field separator '|' is supported"
    );
    let mut segments = Vec::new();
    for line in text.split(['\r', '\n']).filter(|l| !l.is_empty()) {
        ensure!(
            segments.len() < MAX_SEGMENTS,
            "HL7 message exceeds {MAX_SEGMENTS} segments"
        );
        let mut parts = line.split(separator);
        let id = parts.next().unwrap_or_default().to_owned();
        segment_id(&id)?;
        let fields: Vec<String> = parts.map(str::to_owned).collect();
        ensure!(
            fields.len() <= MAX_FIELDS,
            "segment {id} exceeds {MAX_FIELDS} fields"
        );
        ensure!(
            fields
                .iter()
                .all(|f| f.len() <= MAX_FIELD_BYTES && !f.chars().any(|c| c.is_control())),
            "segment {id} has an over-long field or a control character"
        );
        segments.push(Segment { id, fields });
    }
    ensure!(segments[0].id == "MSH", "first segment must be MSH");
    let encoding = segments[0].fields.first().map(String::as_str).unwrap_or("");
    ensure!(
        encoding.starts_with("^~\\&"),
        "MSH-2 must declare the standard encoding characters ^~\\&"
    );
    let message = Message { segments, latin1 };
    ensure!(
        !message.control_id().is_empty(),
        "MSH-10 (message control id) is required"
    );
    ensure!(
        !message.message_type().is_empty(),
        "MSH-9 (message type) is required"
    );
    Ok(message)
}

fn render(segments: &[Segment]) -> Result<Vec<u8>> {
    let mut out = String::new();
    for s in segments {
        segment_id(&s.id)?;
        out.push_str(&s.id);
        for (i, f) in s.fields.iter().enumerate() {
            out.push('|');
            if s.id == "MSH" && i == 0 {
                out.push_str(f);
            } else {
                out.push_str(&field(f)?);
            }
        }
        out.push('\r');
        ensure!(
            out.len() <= MAX_MESSAGE_BYTES,
            "HL7 message exceeds {MAX_MESSAGE_BYTES} bytes"
        );
    }
    Ok(out.into_bytes())
}

/// MLLP-frame a message.
pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 3);
    out.push(START);
    out.extend_from_slice(message);
    out.extend_from_slice(&END);
    out
}

/// HL7 DTM, second precision, UTC.
pub fn timestamp() -> String {
    let secs = crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant), proleptic Gregorian.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Header fields a sender chooses.
pub struct Header<'a> {
    pub sending_application: &'a str,
    pub sending_facility: &'a str,
    pub receiving_application: &'a str,
    pub receiving_facility: &'a str,
    pub message_type: &'a str,
    pub control_id: &'a str,
    pub processing_id: &'a str,
    pub version: &'a str,
}

pub fn msh(h: &Header) -> Result<Segment> {
    for (name, v) in [
        ("message_type", h.message_type),
        ("control_id", h.control_id),
        ("version", h.version),
    ] {
        ensure!(!v.is_empty(), "{name} is required");
    }
    ensure!(
        h.control_id.len() <= 199,
        "control id exceeds 199 characters"
    );
    let fields = [
        "^~\\&",
        h.sending_application,
        h.sending_facility,
        h.receiving_application,
        h.receiving_facility,
        &timestamp(),
        "",
        h.message_type,
        h.control_id,
        h.processing_id,
        h.version,
    ];
    Ok(Segment {
        id: "MSH".into(),
        fields: fields.iter().map(|s| s.to_string()).collect(),
    })
}

/// Build a message from a header and body segments (no MSH among them).
pub fn build(header: &Header, body: &[Segment]) -> Result<Vec<u8>> {
    ensure!(
        body.len() < MAX_SEGMENTS,
        "HL7 message exceeds {MAX_SEGMENTS} segments"
    );
    ensure!(
        body.iter().all(|s| s.id != "MSH"),
        "MSH is built by Rust; do not supply it"
    );
    let mut segments = vec![msh(header)?];
    segments.extend(body.iter().cloned());
    render(&segments)
}

pub const ACK_CODES: &[&str] = &["AA", "AE", "AR", "CA", "CE", "CR"];

/// The acknowledgment for `original`: MSH with sender and receiver swapped, ACK^<trigger>^ACK,
/// MSA echoing the original control id, optional ERR, then any extra body segments.
pub fn ack(
    original: &Message,
    code: &str,
    text: &str,
    control_id: &str,
    error: Option<&Value>,
    extra: &[Segment],
) -> Result<Vec<u8>> {
    ensure!(
        ACK_CODES.contains(&code),
        "acknowledgment code must be one of {ACK_CODES:?}"
    );
    let trigger = original.message_type().split('^').nth(1).unwrap_or("");
    let ack_type = if trigger.is_empty() {
        "ACK".to_owned()
    } else {
        format!("ACK^{trigger}^ACK")
    };
    let header = Header {
        sending_application: original.msh(5),
        sending_facility: original.msh(6),
        receiving_application: original.msh(3),
        receiving_facility: original.msh(4),
        message_type: &ack_type,
        control_id,
        processing_id: original.msh(11),
        version: if original.msh(12).is_empty() {
            "2.5"
        } else {
            original.msh(12)
        },
    };
    let mut body = vec![Segment {
        id: "MSA".into(),
        fields: vec![code.into(), original.control_id().into(), text.into()],
    }];
    if let Some(e) = error {
        let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        // ERR-3 HL7 error code (CWE), ERR-4 severity (E/W/I), ERR-8 user message.
        body.push(Segment {
            id: "ERR".into(),
            fields: vec![
                String::new(),
                s("location"),
                s("code"),
                s("severity"),
                String::new(),
                String::new(),
                String::new(),
                s("message"),
            ],
        });
    }
    body.extend(extra.iter().cloned());
    build(&header, &body)
}

/// Read one MLLP frame: bytes before the start block are refused, the length is bounded
/// before it grows, and the end block must be `<FS><CR>`.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    idle: Duration,
) -> Result<Option<Vec<u8>>> {
    let mut byte = [0u8; 1];
    let n = tokio::time::timeout(idle, reader.read(&mut byte))
        .await
        .context("MLLP idle deadline")??;
    if n == 0 {
        return Ok(None);
    }
    ensure!(byte[0] == START, "MLLP frame must begin with <VT> (0x0B)");
    tokio::time::timeout(IO_TIMEOUT, async {
        let mut message = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                bail!("peer closed inside an MLLP frame");
            }
            message.extend_from_slice(&buf[..n]);
            if let Some(end) = message.windows(2).position(|w| w == END) {
                ensure!(
                    end + 2 == message.len(),
                    "bytes after the MLLP end block (one message per frame, one frame in flight)"
                );
                message.truncate(end);
                return Ok(Some(message));
            }
            ensure!(
                !message.contains(&0x1c),
                "<FS> without <CR> inside an MLLP frame"
            );
            ensure!(!message.contains(&START), "<VT> inside an MLLP frame");
            ensure!(
                message.len() <= MAX_MESSAGE_BYTES + 2,
                "MLLP frame exceeds {MAX_MESSAGE_BYTES} bytes"
            );
        }
    })
    .await
    .context("MLLP whole-frame deadline")?
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, message: &[u8]) -> Result<usize> {
    let framed = frame(message);
    tokio::time::timeout(IO_TIMEOUT, async {
        writer.write_all(&framed).await?;
        writer.flush().await
    })
    .await
    .context("MLLP write deadline")??;
    Ok(framed.len())
}

pub fn segments_from(v: &Value) -> Result<Vec<Segment>> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(_) => {
            let segments: Vec<Segment> = serde_json::from_value(v.clone())
                .context("segments must be [{id, fields: [text]}]")?;
            ensure!(segments.len() < MAX_SEGMENTS, "too many segments");
            for s in &segments {
                segment_id(&s.id)?;
                ensure!(
                    s.fields.len() <= MAX_FIELDS,
                    "segment {} exceeds {MAX_FIELDS} fields",
                    s.id
                );
                for f in &s.fields {
                    field(f)?;
                }
            }
            Ok(segments)
        }
        _ => bail!("segments must be an array"),
    }
}
