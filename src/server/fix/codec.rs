//! FIX tag=value framing (FIX 4.x and FIXT.1.1 session layer): BeginString, BodyLength and
//! CheckSum are checked here, length-prefixed data fields may carry SOH, and repeated tags
//! (repeating groups) keep their order.
use anyhow::{bail, ensure, Context, Result};

pub const SOH: u8 = 0x01;
/// Largest BodyLength accepted, and the bound on buffered unparsed input.
pub const MAX_BODY: usize = 256 * 1024;
pub const MAX_FIELDS: usize = 2000;

/// Length tag → the data tag it measures (FIX 4.4 data fields).
const DATA_PAIRS: &[(u32, u32)] = &[
    (90, 91),
    (93, 89),
    (95, 96),
    (212, 213),
    (348, 349),
    (350, 351),
    (352, 353),
    (354, 355),
    (356, 357),
    (358, 359),
    (360, 361),
    (362, 363),
    (364, 365),
    (445, 446),
    (618, 619),
    (621, 622),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Every field in wire order, BeginString through CheckSum excluded.
    pub fields: Vec<(u32, String)>,
}

impl Message {
    pub fn get(&self, tag: u32) -> Option<&str> {
        self.fields
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_str())
    }
    pub fn msg_type(&self) -> &str {
        self.get(35).unwrap_or_default()
    }
    pub fn seq(&self) -> Option<u32> {
        self.get(34).and_then(|s| s.parse().ok())
    }
}

/// What the framer found at the front of the buffer.
pub enum Frame {
    /// Need more bytes.
    Incomplete,
    /// A whole message; `consumed` bytes may be dropped.
    Message {
        begin: String,
        message: Message,
        consumed: usize,
    },
    /// `consumed` bytes are garbled (FIX: ignore them and carry on).
    Garbled { consumed: usize, reason: String },
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))
}

/// Frame one message from the front of `buf`.
pub fn frame(buf: &[u8]) -> Frame {
    if buf.is_empty() {
        return Frame::Incomplete;
    }
    if !buf.starts_with(b"8=") {
        // Resynchronise on the next BeginString after a field separator.
        return match find(buf, b"\x018=", 0) {
            Some(p) => Frame::Garbled {
                consumed: p + 1,
                reason: "bytes before BeginString".into(),
            },
            None if buf.len() > 1 => Frame::Garbled {
                consumed: buf.len() - 1,
                reason: "no BeginString".into(),
            },
            None => Frame::Incomplete,
        };
    }
    let Some(e1) = buf.iter().position(|b| *b == SOH) else {
        return if buf.len() > 32 {
            Frame::Garbled {
                consumed: 2,
                reason: "BeginString too long".into(),
            }
        } else {
            Frame::Incomplete
        };
    };
    let begin = String::from_utf8_lossy(&buf[2..e1]).into_owned();
    let rest = &buf[e1 + 1..];
    if rest.len() < 2 {
        return Frame::Incomplete;
    }
    if !rest.starts_with(b"9=") {
        return Frame::Garbled {
            consumed: e1 + 1,
            reason: "BodyLength is not the second field".into(),
        };
    }
    let Some(e2) = rest.iter().position(|b| *b == SOH) else {
        return if rest.len() > 12 {
            Frame::Garbled {
                consumed: e1 + 1,
                reason: "BodyLength too long".into(),
            }
        } else {
            Frame::Incomplete
        };
    };
    let len: usize = match std::str::from_utf8(&rest[2..e2])
        .ok()
        .filter(|s| !s.is_empty() && s.len() <= 7 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
    {
        Some(n) if n <= MAX_BODY => n,
        _ => {
            return Frame::Garbled {
                consumed: e1 + 1 + e2 + 1,
                reason: "BodyLength is not a number within bounds".into(),
            }
        }
    };
    let body_start = e1 + 1 + e2 + 1;
    let total = body_start + len + 7;
    if buf.len() < total {
        return Frame::Incomplete;
    }
    let trailer = &buf[body_start + len..total];
    if !trailer.starts_with(b"10=")
        || trailer[6] != SOH
        || !trailer[3..6].iter().all(u8::is_ascii_digit)
    {
        return Frame::Garbled {
            consumed: body_start,
            reason: "BodyLength does not end at CheckSum".into(),
        };
    }
    let declared: u32 = std::str::from_utf8(&trailer[3..6])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    if declared != checksum(&buf[..body_start + len]) as u32 {
        return Frame::Garbled {
            consumed: total,
            reason: "CheckSum mismatch".into(),
        };
    }
    match parse_fields(&buf[body_start..body_start + len]) {
        Ok(fields) => Frame::Message {
            begin,
            message: Message { fields },
            consumed: total,
        },
        Err(e) => Frame::Garbled {
            consumed: total,
            reason: format!("{e:#}"),
        },
    }
}

fn parse_fields(body: &[u8]) -> Result<Vec<(u32, String)>> {
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut i = 0;
    let mut data_len: Option<(u32, usize)> = None;
    while i < body.len() {
        ensure!(out.len() < MAX_FIELDS, "more than {MAX_FIELDS} fields");
        let eq = body[i..]
            .iter()
            .position(|b| *b == b'=')
            .context("field without =")?
            + i;
        let tag_s = std::str::from_utf8(&body[i..eq])
            .ok()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 9
                    && !s.starts_with('0')
                    && s.bytes().all(|b| b.is_ascii_digit())
            })
            .context("invalid tag")?;
        let tag: u32 = tag_s.parse()?;
        let value = match data_len.take() {
            Some((data_tag, n)) if data_tag == tag => {
                let v = body
                    .get(eq + 1..eq + 1 + n)
                    .context("data field shorter than its length")?;
                ensure!(
                    body.get(eq + 1 + n) == Some(&SOH),
                    "data field not followed by SOH"
                );
                i = eq + 1 + n + 1;
                String::from_utf8_lossy(v).into_owned()
            }
            _ => {
                let end = body[eq + 1..]
                    .iter()
                    .position(|b| *b == SOH)
                    .context("field not terminated by SOH")?
                    + eq
                    + 1;
                let v = &body[eq + 1..end];
                ensure!(!v.is_empty(), "tag {tag} has an empty value");
                i = end + 1;
                String::from_utf8_lossy(v).into_owned()
            }
        };
        if let Some((_, data_tag)) = DATA_PAIRS.iter().find(|(l, _)| *l == tag) {
            let n: usize = value
                .parse()
                .ok()
                .filter(|n| *n <= MAX_BODY)
                .context("invalid data length")?;
            data_len = Some((*data_tag, n));
        }
        out.push((tag, value));
    }
    ensure!(
        !out.is_empty() && out[0].0 == 35,
        "MsgType is not the third field"
    );
    Ok(out)
}

/// Encode a message: BeginString, BodyLength, `fields` (MsgType first), CheckSum.
pub fn encode(begin: &str, fields: &[(u32, String)]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for (tag, value) in fields {
        if value.as_bytes().contains(&SOH) && !DATA_PAIRS.iter().any(|(_, d)| d == tag) {
            bail!("tag {tag} contains SOH");
        }
        body.extend_from_slice(format!("{tag}=").as_bytes());
        body.extend_from_slice(value.as_bytes());
        body.push(SOH);
    }
    ensure!(body.len() <= MAX_BODY, "message body over {MAX_BODY} bytes");
    let mut out = format!("8={begin}\x019={}\x01", body.len()).into_bytes();
    out.extend(body);
    let sum = checksum(&out);
    out.extend_from_slice(format!("10={sum:03}\x01").as_bytes());
    Ok(out)
}

/// FIX UTCTimestamp with milliseconds.
pub fn timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}:{:02}:{:02}.{:03}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}
