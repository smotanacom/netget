//! STOMP 1.2 frame codec.
//!
//! A STOMP frame is
//!
//! ```text
//! COMMAND\n
//! header:value\n
//! ...\n
//! \n
//! <body>\0
//! ```
//!
//! The framing is done here, in Rust, rather than handed to the model: frame boundaries,
//! header escaping and the NUL terminator are mechanical, and a model that gets any of them
//! wrong produces a stream a real client cannot resynchronise from. What the model decides is
//! the *content* — which is what the events and actions in `actions.rs` carry.
//!
//! Three details of the spec that are easy to get wrong and are implemented here:
//!
//! - **Header escaping is per-frame, not global.** `\r` `\n` `:` `\` are escaped as `\\r`
//!   `\\n` `\\c` `\\\\` in every frame *except* `CONNECT`, `STOMP` and `CONNECTED`, which are
//!   exempt for backwards compatibility with STOMP 1.0/1.1 clients. [`should_escape`] is the
//!   single place that decision is made, so encode and decode cannot disagree.
//! - **An undefined escape sequence is a fatal protocol error**, not something to pass
//!   through. [`unescape_header`] returns [`FrameError::InvalidEscape`].
//! - **`content-length`, when present, is authoritative** — the body may then contain NUL
//!   bytes, and the terminator is the byte *after* the counted body. Without it the body runs
//!   to the first NUL.
//!
//! Repeated headers: the first occurrence wins ([`StompFrame::header`]), which is what the
//! spec requires. Order is preserved so a frame can be logged as it arrived.

/// Largest frame this server will buffer before giving up on a peer.
///
/// Without a bound, a peer that opens a frame and never terminates it makes the server grow a
/// buffer forever. One megabyte is far above anything the actions here can produce and far
/// below anything that threatens the process.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Largest number of header lines accepted in one frame.
///
/// `MAX_FRAME_BYTES` bounds the *memory*, but not the work: the connection loop re-parses the
/// whole pending buffer after every read, allocating two `String`s per header line each time.
/// One megabyte of three-byte header lines (`a:\n`) is ~350k headers re-parsed ~128 times as
/// the 8 KB reads arrive - tens of millions of allocations from a megabyte of input, per
/// connection, with no connection limit. No real STOMP frame has more than a dozen headers.
pub const MAX_HEADERS: usize = 1024;

/// Longest peer-supplied fragment quoted back in a `FrameError`.
///
/// A `FrameError` reaches three places: the `ERROR` frame sent to the peer, `netget.log`, and
/// the TUI status stream, which is an unbounded channel with no backpressure. Quoting a
/// megabyte header line verbatim - which `{:?}` escaping can roughly double - into all three
/// is a peer's choice of how much memory to spend on NetGet's behalf.
const MAX_QUOTED_FRAGMENT: usize = 120;

/// A parsed STOMP frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StompFrame {
    /// The command line (`CONNECT`, `SEND`, `MESSAGE`, …), verbatim.
    pub command: String,
    /// Headers in arrival order, already unescaped where the command calls for it.
    pub headers: Vec<(String, String)>,
    /// The frame body, exactly as it appeared on the wire.
    pub body: Vec<u8>,
}

impl StompFrame {
    /// Build a frame from borrowed parts.
    pub fn new(command: impl Into<String>, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            command: command.into(),
            headers,
            body,
        }
    }

    /// First value for `name`, which is the one the spec says applies.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every header except the ones listed, as a JSON object.
    ///
    /// Used to hand a model the headers it has not already been given as named event fields,
    /// without ever handing it raw bytes.
    pub fn headers_except(&self, exclude: &[&str]) -> serde_json::Map<String, serde_json::Value> {
        let mut out = serde_json::Map::new();
        for (k, v) in &self.headers {
            if exclude.contains(&k.as_str()) {
                continue;
            }
            out.entry(k.clone())
                .or_insert_with(|| serde_json::Value::String(v.clone()));
        }
        out
    }

    /// Serialise to wire bytes.
    ///
    /// `content-length` is added automatically for a non-empty body when the caller did not
    /// supply one: without it a body containing a NUL byte would truncate the frame, and the
    /// caller cannot always know whether the model's text is binary-safe.
    pub fn encode(&self) -> Vec<u8> {
        let escape = should_escape(&self.command);
        let mut out = Vec::with_capacity(64 + self.body.len());
        out.extend_from_slice(self.command.as_bytes());
        out.push(b'\n');

        let mut has_content_length = false;
        for (k, v) in &self.headers {
            if k == "content-length" {
                has_content_length = true;
            }
            let (k, v) = if escape {
                (escape_header(k), escape_header(v))
            } else {
                (k.clone(), v.clone())
            };
            out.extend_from_slice(k.as_bytes());
            out.push(b':');
            out.extend_from_slice(v.as_bytes());
            out.push(b'\n');
        }

        if !has_content_length && !self.body.is_empty() {
            out.extend_from_slice(format!("content-length:{}\n", self.body.len()).as_bytes());
        }

        out.push(b'\n');
        out.extend_from_slice(&self.body);
        out.push(0);
        out
    }
}

/// What a parse attempt found in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    /// A complete frame, and how many bytes of the buffer it occupied.
    Frame {
        /// The frame.
        frame: StompFrame,
        /// Bytes to drain from the front of the buffer.
        consumed: usize,
    },
    /// Only inter-frame EOLs were present. STOMP uses a bare newline as a heart-beat, and a
    /// client is also allowed to leave trailing EOLs after a frame's NUL, so these are drained
    /// and ignored rather than treated as an empty command.
    Heartbeat {
        /// Bytes to drain from the front of the buffer.
        consumed: usize,
    },
    /// Not enough bytes yet; read more and try again.
    Incomplete,
}

/// A frame the peer sent that cannot be interpreted. Every variant is fatal for the
/// connection: STOMP has no way to resynchronise a stream once framing is lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// A header line with no `:` separator.
    MalformedHeader(String),
    /// A `\` followed by something other than `r`, `n`, `c` or `\`.
    InvalidEscape(String),
    /// `content-length` was present but not a non-negative integer.
    BadContentLength(String),
    /// `content-length` bytes were followed by something other than NUL.
    MissingNulTerminator,
    /// The command line or a header was not valid UTF-8.
    NotUtf8,
    /// The peer exceeded [`MAX_FRAME_BYTES`] without completing a frame.
    FrameTooLarge(usize),
    /// The frame declared more than [`MAX_HEADERS`] header lines.
    TooManyHeaders,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::MalformedHeader(line) => {
                write!(f, "header line has no ':' separator: {line:?}")
            }
            FrameError::InvalidEscape(seq) => write!(f, "undefined header escape sequence {seq:?}"),
            FrameError::BadContentLength(v) => {
                write!(f, "content-length is not a non-negative integer: {v:?}")
            }
            FrameError::MissingNulTerminator => write!(
                f,
                "content-length bytes were not followed by a NUL terminator"
            ),
            FrameError::NotUtf8 => write!(f, "command line or header was not valid UTF-8"),
            FrameError::FrameTooLarge(n) => write!(
                f,
                "frame exceeded {MAX_FRAME_BYTES} bytes without terminating ({n} buffered)"
            ),
            FrameError::TooManyHeaders => {
                write!(f, "frame declared more than {MAX_HEADERS} headers")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Trim a peer-supplied fragment to something safe to quote back at it and into the logs.
///
/// `crate::utils::truncate_for_log` rather than `&s[..N]`, because a header value is arbitrary
/// UTF-8 and a byte-index cut lands mid-character often enough to matter.
fn quote_fragment(s: &str) -> String {
    crate::utils::truncate_for_log(s, MAX_QUOTED_FRAGMENT)
}

/// Whether a header name or value can be written into a frame whose command is exempt from
/// escaping, without forging a header.
///
/// `CONNECT`, `STOMP` and `CONNECTED` carry their headers raw (see [`should_escape`]), so for
/// those three commands nothing downstream can neutralise a `\n` or a `:`. A value containing
/// one would inject an extra header, and `\n\n` would terminate the header block and start a
/// body. Rejecting is right rather than escaping: a 1.0/1.1 peer would read an escape sequence
/// literally, which is the whole reason those commands are exempt.
pub fn is_safe_unescaped_header(s: &str) -> bool {
    !s.contains(['\r', '\n', ':', '\0'])
}

/// Whether headers in `command` are escaped.
///
/// `CONNECT`, `STOMP` and `CONNECTED` are exempt in STOMP 1.2 so that a 1.2 endpoint can still
/// read the handshake of a 1.0/1.1 peer, which knew nothing about escaping.
pub fn should_escape(command: &str) -> bool {
    !matches!(command, "CONNECT" | "STOMP" | "CONNECTED")
}

/// Encode the four sequences STOMP 1.2 defines. Nothing else is touched.
pub fn escape_header(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            ':' => out.push_str("\\c"),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

/// Decode the four sequences STOMP 1.2 defines.
///
/// Any other sequence is a fatal error by the spec — passing it through would let a peer smuggle
/// a `:` or a newline past the parser and forge a header.
pub fn unescape_header(s: &str) -> Result<String, FrameError> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('c') => out.push(':'),
            Some('\\') => out.push('\\'),
            Some(other) => return Err(FrameError::InvalidEscape(format!("\\{other}"))),
            None => return Err(FrameError::InvalidEscape("\\<end of value>".to_string())),
        }
    }
    Ok(out)
}

/// Try to take one frame off the front of `buf`.
///
/// Never consumes a partial frame: on [`ParseOutcome::Incomplete`] the caller keeps the buffer
/// as it is and reads more.
pub fn parse_frame(buf: &[u8]) -> Result<ParseOutcome, FrameError> {
    match parse_inner(buf) {
        Ok(ParseOutcome::Incomplete) if buf.len() > MAX_FRAME_BYTES => {
            Err(FrameError::FrameTooLarge(buf.len()))
        }
        other => other,
    }
}

fn parse_inner(buf: &[u8]) -> Result<ParseOutcome, FrameError> {
    // Drain inter-frame EOLs (heart-beats, and the trailing newlines some clients append
    // after a frame's NUL). A lone trailing '\r' is ambiguous until the next byte arrives.
    let mut pos = 0usize;
    loop {
        match buf.get(pos) {
            Some(b'\n') => pos += 1,
            Some(b'\r') => match buf.get(pos + 1) {
                Some(b'\n') => pos += 2,
                Some(_) => break,
                None => return Ok(ParseOutcome::Incomplete),
            },
            _ => break,
        }
    }
    if pos > 0 && pos >= buf.len() {
        return Ok(ParseOutcome::Heartbeat { consumed: pos });
    }
    if pos >= buf.len() {
        return Ok(ParseOutcome::Incomplete);
    }

    let Some((command, next)) = read_line(buf, pos)? else {
        return Ok(ParseOutcome::Incomplete);
    };
    pos = next;

    let escape = should_escape(&command);
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let Some((line, next)) = read_line(buf, pos)? else {
            return Ok(ParseOutcome::Incomplete);
        };
        pos = next;
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(FrameError::TooManyHeaders);
        }
        let Some((raw_name, raw_value)) = line.split_once(':') else {
            return Err(FrameError::MalformedHeader(quote_fragment(&line)));
        };
        let (name, value) = if escape {
            (unescape_header(raw_name)?, unescape_header(raw_value)?)
        } else {
            (raw_name.to_string(), raw_value.to_string())
        };
        headers.push((name, value));
    }

    // `content-length` is authoritative when present, so a body may contain NUL bytes.
    let declared_len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .map(|(_, v)| {
            v.trim()
                .parse::<usize>()
                .map_err(|_| FrameError::BadContentLength(quote_fragment(v)))
        })
        .transpose()?;

    let (body, consumed) = match declared_len {
        Some(len) => {
            // `content-length: 18446744073709551615` parses cleanly into a `usize`, and
            // `pos + len` then overflowed: a panic in debug and test builds (there is no
            // `[profile.dev]` override, so `overflow-checks` is on), and in release a wrap to
            // `pos - 1` that made the bug invisible in the shipped profile while it was live
            // in every test run. The panic happened inside the connection's `tokio::spawn`,
            // so it was swallowed, the connection's cleanup never ran, and the peer handle
            // and `AppState` row leaked with the connection stuck `Active`. Reachable before
            // the CONNECT gate, by any peer, in one frame.
            if len > MAX_FRAME_BYTES {
                return Err(FrameError::FrameTooLarge(len));
            }
            let end = pos + len;
            if end >= buf.len() {
                return Ok(ParseOutcome::Incomplete);
            }
            if buf[end] != 0 {
                return Err(FrameError::MissingNulTerminator);
            }
            (buf[pos..end].to_vec(), end + 1)
        }
        None => {
            let Some(offset) = buf[pos..].iter().position(|&b| b == 0) else {
                return Ok(ParseOutcome::Incomplete);
            };
            (buf[pos..pos + offset].to_vec(), pos + offset + 1)
        }
    };

    Ok(ParseOutcome::Frame {
        frame: StompFrame {
            command,
            headers,
            body,
        },
        consumed,
    })
}

/// Read one `\n`- or `\r\n`-terminated line starting at `from`.
///
/// `Ok(None)` means the line is not complete yet.
fn read_line(buf: &[u8], from: usize) -> Result<Option<(String, usize)>, FrameError> {
    let Some(offset) = buf[from..].iter().position(|&b| b == b'\n') else {
        return Ok(None);
    };
    let mut end = from + offset;
    let next = end + 1;
    if end > from && buf[end - 1] == b'\r' {
        end -= 1;
    }
    let line = std::str::from_utf8(&buf[from..end])
        .map_err(|_| FrameError::NotUtf8)?
        .to_string();
    Ok(Some((line, next)))
}

/// A `RECEIPT` frame for `receipt_id`.
pub fn receipt_frame(receipt_id: &str) -> Vec<u8> {
    StompFrame::new(
        "RECEIPT",
        vec![("receipt-id".to_string(), receipt_id.to_string())],
        Vec::new(),
    )
    .encode()
}

/// An `ERROR` frame.
///
/// `message` becomes the `message` header (the short reason) and `body` the explanatory text.
/// Both are caller-chosen, and the callers in this protocol only ever pass literals or a
/// [`crate::utils::WireFailure`] category — an internal error string must never reach here.
pub fn error_frame(message: &str, body: &str) -> Vec<u8> {
    StompFrame::new(
        "ERROR",
        vec![
            ("message".to_string(), message.to_string()),
            ("content-type".to_string(), "text/plain".to_string()),
        ],
        body.as_bytes().to_vec(),
    )
    .encode()
}
