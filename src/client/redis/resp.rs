//! RESP for the Redis client: splitting the model's command line into arguments, and reading
//! one complete server reply off the wire.
//!
//! # Why a reply reader and not a line reader
//!
//! RESP is length-prefixed, not line-oriented. A bulk string is a `$<len>` header line followed
//! by `<len>` bytes that may themselves contain CRLF, and an array is a `*<n>` header followed by
//! `n` further replies. Reading line by line turned `GET k` → `$5\r\nhello\r\n` into **two**
//! events — `"$5"`, then `"hello"` — so the model was asked what to do about a length header,
//! and any value containing a newline arrived in pieces. Every reply here is one event.
//!
//! # Bounds
//!
//! The reader is iterative, so nesting cannot overflow *its* stack — but the `serde_json::Value`
//! it builds is recursive, and serialising or dropping one nested ~10 000 deep (four bytes a
//! level: `*1\r\n`) would. So depth is bounded at [`MAX_REPLY_DEPTH`]; real replies (`XREAD`,
//! `CLUSTER SLOTS`, `COMMAND DOCS`) stay under ten. Every length the server *declares* is
//! checked against [`MAX_REPLY_BYTES`] / [`MAX_AGGREGATE_LEN`] **before** anything is allocated
//! for it, and the whole reply shares one byte budget, so no header can make the client buffer
//! more than [`MAX_REPLY_BYTES`]. A reply that breaks a bound, or is not RESP, is an error: the
//! connection's framing is then unknowable, and the caller closes it.

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Deepest aggregate nesting accepted in one reply.
pub const MAX_REPLY_DEPTH: usize = 32;
/// Most bytes one reply may occupy on the wire, headers included.
pub const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;
/// Most elements one aggregate header may declare.
pub const MAX_AGGREGATE_LEN: usize = 1 << 20;
/// Longest header or simple-string line accepted.
pub const MAX_LINE_LEN: usize = 64 * 1024;

/// One reply, as the model sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// `simple_string`, `error`, `integer`, `bulk_string`, `null`, `array`, `map`, `set`,
    /// `push`, `boolean`, `double`, `big_number` or `verbatim_string`.
    pub reply_type: &'static str,
    /// The reply as JSON: strings, integers, `null`, arrays, objects, and `{"error": "..."}` for
    /// an error (nested errors inside an aggregate take the same shape).
    pub value: Value,
    /// A one-line rendering in `redis-cli`'s style: `OK`, `(integer) 3`, `"v"`, `(nil)`,
    /// `(error) ERR …`; aggregates as JSON.
    pub response: String,
}

impl Reply {
    /// The event data for `redis_response_received`.
    pub fn event_data(&self) -> Value {
        serde_json::json!({
            "response": self.response,
            "reply_type": self.reply_type,
            "value": self.value,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Array,
    Set,
    Push,
    Map,
    /// RESP3 attributes precede the reply they annotate and are not counted as an element of
    /// any aggregate. The map is read, bounded like everything else, and discarded.
    Attribute,
}

struct Frame {
    kind: Kind,
    remaining: usize,
    items: Vec<Value>,
}

/// What one header line produced.
enum Parsed {
    /// A complete value and its reply type.
    Value(&'static str, Value),
    /// An aggregate header: a frame has been pushed.
    Open,
}

/// Read exactly one reply. `Ok(None)` is a clean EOF before the first byte of a reply.
pub async fn read_reply<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Reply>> {
    let mut budget = MAX_REPLY_BYTES;
    let mut stack: Vec<Frame> = Vec::new();
    // The type of the outermost non-attribute value, fixed when its header is read.
    let mut top_type: Option<&'static str> = None;
    let mut started = false;

    loop {
        let Some(line) = read_line(reader, &mut budget).await? else {
            if !started {
                return Ok(None);
            }
            bail!("connection closed in the middle of a reply");
        };
        started = true;

        let at_top = stack.iter().all(|f| f.kind == Kind::Attribute);
        let parsed = parse_header(reader, &line, &mut budget, &mut stack).await?;

        let (mut kind, mut value) = match parsed {
            Parsed::Open => {
                let opened = stack.last().expect("parse_header pushed a frame");
                if at_top && top_type.is_none() && opened.kind != Kind::Attribute {
                    top_type = Some(kind_name(opened.kind));
                }
                if opened.remaining > 0 {
                    continue;
                }
                let frame = stack.pop().expect("just checked");
                (Some(frame.kind), finish(frame))
            }
            Parsed::Value(t, v) => {
                if at_top && top_type.is_none() {
                    top_type = Some(t);
                }
                (None, v)
            }
        };

        // Deliver the value upward, folding every aggregate it completes.
        loop {
            if kind == Some(Kind::Attribute) {
                // Discarded, and not an element of its parent.
                break;
            }
            match stack.last_mut() {
                None => {
                    let reply_type = top_type.unwrap_or("null");
                    return Ok(Some(Reply {
                        reply_type,
                        response: render(reply_type, &value),
                        value,
                    }));
                }
                Some(frame) => {
                    frame.items.push(value);
                    frame.remaining -= 1;
                    if frame.remaining > 0 {
                        break;
                    }
                    let frame = stack.pop().expect("just checked");
                    kind = Some(frame.kind);
                    value = finish(frame);
                }
            }
        }
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Array => "array",
        Kind::Set => "set",
        Kind::Push => "push",
        Kind::Map => "map",
        Kind::Attribute => "attribute",
    }
}

/// Interpret one header line. Strings read their body here; aggregates push a frame.
async fn parse_header<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &[u8],
    budget: &mut usize,
    stack: &mut Vec<Frame>,
) -> Result<Parsed> {
    let (marker, rest) = line.split_at(1);
    let rest = std::str::from_utf8(rest)
        .map_err(|_| anyhow!("non-UTF-8 RESP header"))?
        .to_string();

    Ok(match marker[0] {
        b'+' => Parsed::Value("simple_string", Value::String(rest)),
        b'-' => Parsed::Value("error", serde_json::json!({ "error": rest })),
        b':' => Parsed::Value("integer", Value::from(parse_int(&rest)?)),
        b'_' => Parsed::Value("null", Value::Null),
        b'#' => match rest.as_str() {
            "t" => Parsed::Value("boolean", Value::Bool(true)),
            "f" => Parsed::Value("boolean", Value::Bool(false)),
            other => bail!("bad RESP3 boolean {other:?}"),
        },
        b',' => Parsed::Value(
            "double",
            rest.parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::String(rest)),
        ),
        b'(' => Parsed::Value("big_number", Value::String(rest)),
        b'$' | b'!' | b'=' => {
            let declared = parse_int(&rest)?;
            if declared < 0 {
                return Ok(Parsed::Value("null", Value::Null));
            }
            let len = declared as usize;
            // The declared length is checked before a byte is allocated for it.
            if len.saturating_add(2) > *budget {
                bail!(
                    "reply declares a {len}-byte string, over the {MAX_REPLY_BYTES}-byte reply \
                     limit"
                );
            }
            *budget -= len + 2;
            let mut body = vec![0u8; len + 2];
            reader.read_exact(&mut body).await?;
            if &body[len..] != b"\r\n" {
                bail!("string body not terminated by CRLF");
            }
            body.truncate(len);
            let mut text = String::from_utf8_lossy(&body).to_string();
            match marker[0] {
                b'!' => Parsed::Value("error", serde_json::json!({ "error": text })),
                b'=' => {
                    // Verbatim strings carry a three-letter format prefix (`txt:`).
                    if text.len() >= 4 && text.as_bytes()[3] == b':' {
                        text = text[4..].to_string();
                    }
                    Parsed::Value("verbatim_string", Value::String(text))
                }
                _ => Parsed::Value("bulk_string", Value::String(text)),
            }
        }
        b'*' | b'~' | b'>' | b'%' | b'|' => {
            let declared = parse_int(&rest)?;
            if declared < 0 {
                return Ok(Parsed::Value("null", Value::Null));
            }
            let n = declared as usize;
            if n > MAX_AGGREGATE_LEN {
                bail!("reply declares {n} elements, over the {MAX_AGGREGATE_LEN} limit");
            }
            if stack.len() >= MAX_REPLY_DEPTH {
                bail!("reply nests deeper than {MAX_REPLY_DEPTH} levels");
            }
            let kind = match marker[0] {
                b'*' => Kind::Array,
                b'~' => Kind::Set,
                b'>' => Kind::Push,
                b'%' => Kind::Map,
                _ => Kind::Attribute,
            };
            let remaining = match kind {
                Kind::Map | Kind::Attribute => n * 2,
                _ => n,
            };
            stack.push(Frame {
                kind,
                remaining,
                items: Vec::with_capacity(remaining.min(1024)),
            });
            Parsed::Open
        }
        other => bail!("not a RESP reply: unexpected type byte 0x{other:02x}"),
    })
}

fn finish(frame: Frame) -> Value {
    match frame.kind {
        Kind::Array | Kind::Set | Kind::Push => Value::Array(frame.items),
        Kind::Map => {
            let mut map = Map::new();
            let mut it = frame.items.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                let key = match k {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                map.insert(key, v);
            }
            Value::Object(map)
        }
        Kind::Attribute => Value::Null,
    }
}

fn render(reply_type: &str, value: &Value) -> String {
    match (reply_type, value) {
        (_, Value::Null) => "(nil)".to_string(),
        ("simple_string" | "big_number", Value::String(s)) => s.clone(),
        ("error", Value::Object(o)) => format!(
            "(error) {}",
            o.get("error").and_then(|e| e.as_str()).unwrap_or("")
        ),
        ("integer", v) => format!("(integer) {v}"),
        ("boolean", Value::Bool(b)) => format!("({b})"),
        ("double", v) => format!("(double) {v}"),
        (_, Value::String(s)) => Value::String(s.clone()).to_string(),
        (_, v) => v.to_string(),
    }
}

fn parse_int(text: &str) -> Result<i64> {
    text.trim()
        .parse::<i64>()
        .map_err(|_| anyhow!("bad RESP length or integer {text:?}"))
}

/// One CRLF-terminated line, without the terminator. Bounded by [`MAX_LINE_LEN`] and charged to
/// the reply's byte budget. `None` on EOF before any byte.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    budget: &mut usize,
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            bail!("connection closed in the middle of a line");
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if line.len() + take > MAX_LINE_LEN {
            bail!("RESP line longer than {MAX_LINE_LEN} bytes");
        }
        if line.len() + take > *budget {
            bail!("reply exceeds the {MAX_REPLY_BYTES}-byte limit");
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if done {
            *budget -= line.len();
            if line.len() < 3 || line[line.len() - 2] != b'\r' {
                bail!("RESP line not terminated by CRLF");
            }
            line.truncate(line.len() - 2);
            return Ok(Some(line));
        }
    }
}

/// Split a command line the way `redis-cli` does (its `sdssplitargs`): whitespace separates
/// arguments; `"…"` quotes allow `\n \r \t \b \a \\ \"` and `\xHH`; `'…'` quotes allow `\'`; a
/// closing quote must be followed by whitespace or the end.
///
/// So `SET greeting "hello world"` is three arguments, not four with stray quotes — which is
/// what the model writes, because it is what `redis-cli` accepts. Unbalanced quotes are an
/// error rather than a guess.
pub fn split_command(line: &str) -> Result<Vec<Vec<u8>>> {
    let bytes = line.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let mut arg = Vec::new();
        let mut in_double = false;
        let mut in_single = false;
        loop {
            if in_double {
                if i >= bytes.len() {
                    bail!("unterminated double quote in Redis command");
                }
                match bytes[i] {
                    b'\\'
                        if i + 3 < bytes.len()
                            && bytes[i + 1] == b'x'
                            && bytes[i + 2].is_ascii_hexdigit()
                            && bytes[i + 3].is_ascii_hexdigit() =>
                    {
                        let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or("00");
                        arg.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                        i += 3;
                    }
                    b'\\' if i + 1 < bytes.len() => {
                        i += 1;
                        arg.push(match bytes[i] {
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'b' => 0x08,
                            b'a' => 0x07,
                            other => other,
                        });
                    }
                    b'"' => {
                        if i + 1 < bytes.len() && !bytes[i + 1].is_ascii_whitespace() {
                            bail!("closing quote must be followed by a space in Redis command");
                        }
                        i += 1;
                        break;
                    }
                    other => arg.push(other),
                }
                i += 1;
            } else if in_single {
                if i >= bytes.len() {
                    bail!("unterminated single quote in Redis command");
                }
                match bytes[i] {
                    b'\\' if i + 1 < bytes.len() && bytes[i + 1] == b'\'' => {
                        arg.push(b'\'');
                        i += 1;
                    }
                    b'\'' => {
                        if i + 1 < bytes.len() && !bytes[i + 1].is_ascii_whitespace() {
                            bail!("closing quote must be followed by a space in Redis command");
                        }
                        i += 1;
                        break;
                    }
                    other => arg.push(other),
                }
                i += 1;
            } else {
                if i >= bytes.len() || bytes[i].is_ascii_whitespace() {
                    break;
                }
                match bytes[i] {
                    b'"' => in_double = true,
                    b'\'' => in_single = true,
                    other => arg.push(other),
                }
                i += 1;
            }
        }
        args.push(arg);
    }
    if args.is_empty() {
        bail!("empty Redis command");
    }
    Ok(args)
}

/// Encode arguments as a RESP array of bulk strings — the form every Redis server accepts.
pub fn encode_command(args: &[Vec<u8>]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}
