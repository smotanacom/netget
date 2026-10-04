//! ManageSieve (RFC 5804) lines: atoms, numbers, quoted strings and literals (`{n}` and
//! `{n+}`), parsed incrementally so a literal may span reads, and the response forms.
use anyhow::{bail, ensure, Result};

/// A line, not counting literal contents.
pub const MAX_LINE: usize = 8 * 1024;
/// One literal (a script).
pub const MAX_LITERAL: usize = 1024 * 1024;
pub const MAX_ARGS: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Atom(String),
    Number(u64),
    String(Vec<u8>),
}

impl Arg {
    pub fn text(&self) -> Option<String> {
        match self {
            Arg::String(b) => String::from_utf8(b.clone()).ok(),
            Arg::Atom(a) => Some(a.clone()),
            Arg::Number(n) => Some(n.to_string()),
        }
    }
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Arg::String(b) => Some(b),
            _ => None,
        }
    }
}

/// One whole line of arguments at the front of `buf`, and how many bytes it used; None when
/// more bytes are needed.
pub fn parse(buf: &[u8]) -> Result<Option<(Vec<Arg>, usize)>> {
    let mut i = 0;
    let mut args = Vec::new();
    let mut line_bytes = 0;
    loop {
        while i < buf.len() && buf[i] == b' ' {
            i += 1;
            line_bytes += 1;
        }
        ensure!(
            line_bytes <= MAX_LINE,
            "the line is longer than {MAX_LINE} bytes"
        );
        if i >= buf.len() {
            return Ok(None);
        }
        match buf[i] {
            b'\r' => {
                if i + 1 >= buf.len() {
                    return Ok(None);
                }
                ensure!(buf[i + 1] == b'\n', "a bare CR");
                return Ok(Some((args, i + 2)));
            }
            b'\n' => return Ok(Some((args, i + 1))),
            b'"' => {
                let mut s = Vec::new();
                let mut j = i + 1;
                loop {
                    if j >= buf.len() {
                        ensure!(
                            j - i <= MAX_LINE,
                            "a quoted string is longer than {MAX_LINE} bytes"
                        );
                        return Ok(None);
                    }
                    match buf[j] {
                        b'"' => break,
                        b'\\' => {
                            if j + 1 >= buf.len() {
                                return Ok(None);
                            }
                            ensure!(
                                matches!(buf[j + 1], b'"' | b'\\'),
                                "a bad escape in a quoted string"
                            );
                            s.push(buf[j + 1]);
                            j += 2;
                        }
                        b'\r' | b'\n' => bail!("a line break inside a quoted string"),
                        c => {
                            s.push(c);
                            j += 1;
                        }
                    }
                }
                line_bytes += j + 1 - i;
                args.push(Arg::String(s));
                i = j + 1;
            }
            b'{' => {
                let Some(close) = buf[i..].iter().position(|c| *c == b'}') else {
                    ensure!(buf.len() - i <= 32, "an unterminated literal length");
                    return Ok(None);
                };
                let spec = std::str::from_utf8(&buf[i + 1..i + close])?;
                let digits = spec.strip_suffix('+').unwrap_or(spec);
                ensure!(
                    !digits.is_empty()
                        && digits.len() <= 10
                        && digits.bytes().all(|b| b.is_ascii_digit()),
                    "a bad literal length"
                );
                let n: usize = digits.parse()?;
                ensure!(
                    n <= MAX_LITERAL,
                    "a literal of {n} bytes is over the {MAX_LITERAL}-byte bound"
                );
                let mut j = i + close + 1;
                if j + 2 > buf.len() {
                    return Ok(None);
                }
                ensure!(
                    &buf[j..j + 2] == b"\r\n",
                    "a literal length not followed by CRLF"
                );
                j += 2;
                if j + n > buf.len() {
                    return Ok(None);
                }
                args.push(Arg::String(buf[j..j + n].to_vec()));
                line_bytes += close + 3;
                i = j + n;
            }
            _ => {
                let start = i;
                while i < buf.len() && !matches!(buf[i], b' ' | b'\r' | b'\n') {
                    ensure!(
                        buf[i].is_ascii_graphic() && !matches!(buf[i], b'"' | b'{' | b'}'),
                        "a bad character in an atom"
                    );
                    i += 1;
                }
                if i >= buf.len() {
                    ensure!(
                        i - start <= MAX_LINE,
                        "an atom is longer than {MAX_LINE} bytes"
                    );
                    return Ok(None);
                }
                let word = std::str::from_utf8(&buf[start..i])?.to_owned();
                line_bytes += i - start;
                args.push(match word.parse::<u64>() {
                    Ok(n) if word.bytes().all(|b| b.is_ascii_digit()) => Arg::Number(n),
                    _ => Arg::Atom(word),
                });
            }
        }
        ensure!(args.len() <= MAX_ARGS, "more than {MAX_ARGS} arguments");
    }
}

/// A string as the server or client writes it: quoted when short and plain, a literal
/// otherwise (`plus` makes it a non-synchronizing client literal).
pub fn string(s: &[u8], plus: bool) -> Vec<u8> {
    let plain = s.len() <= 1024
        && !s
            .iter()
            .any(|c| matches!(c, b'\r' | b'\n' | 0) || *c >= 0x80);
    if plain {
        let mut out = vec![b'"'];
        for c in s {
            if matches!(c, b'"' | b'\\') {
                out.push(b'\\');
            }
            out.push(*c);
        }
        out.push(b'"');
        out
    } else {
        let mut out = format!("{{{}{}}}\r\n", s.len(), if plus { "+" } else { "" }).into_bytes();
        out.extend(s);
        out
    }
}

/// `OK`, `NO` or `BYE` with an optional response code and message.
pub fn status(kind: &str, code: Option<&str>, message: &str) -> Vec<u8> {
    let mut out = kind.as_bytes().to_vec();
    if let Some(c) = code {
        out.extend(format!(" ({c})").into_bytes());
    }
    if !message.is_empty() {
        out.push(b' ');
        out.extend(string(message.as_bytes(), false));
    }
    out.extend(b"\r\n");
    out
}

/// Script names: 1-255 UTF-8 characters with no control characters (RFC 5804 section 1.6).
pub fn valid_name(name: &[u8]) -> bool {
    std::str::from_utf8(name).is_ok_and(|s| {
        !s.is_empty()
            && s.chars().count() <= 255
            && !s
                .chars()
                .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    })
}
