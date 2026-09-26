//! DICT (RFC 2229) wire format: command parsing and response rendering.
//!
//! Everything here is a pure function of its arguments, so the session loop, the action
//! executor and the tests all share one definition of what a DICT line looks like. **The model
//! never writes a byte of framing**: it supplies words, database names and free text, and this
//! module decides the status lines, the quoting, the dot-stuffing and the terminator. A model
//! that writes a definition whose line happens to be `.` produces a line reading `..` on the
//! wire, and the client's un-stuffing turns it back into `.` — not a premature end of the text.

use crate::utils::sanitize;

/// The longest command line a peer may send, **including** the trailing CRLF.
///
/// RFC 2229 §2.2: "Command lines MUST NOT exceed 1024 characters in length, counting all
/// characters including spaces, separators, punctuation, and the trailing CRLF." A line one
/// byte longer is refused with `500` before it is parsed, and before any model is consulted.
pub const MAX_LINE_BYTES: usize = 1024;

/// The longest text line this server writes, before dot-stuffing and CRLF.
///
/// RFC 2229 §2.4.1 holds *text* lines to the same 1024 characters "including … the extra
/// initial period (if needed), and the trailing CRLF", so the hard ceiling is 1021 bytes of
/// content. Wrapping at 1000 leaves room for both with a margin, and a model that writes a
/// 5 KB paragraph as one line gets it split across several lines rather than a line a strict
/// client is entitled to reject.
pub const MAX_TEXT_LINE_CONTENT: usize = 1000;

/// Status codes whose response is followed by a dot-terminated text block.
///
/// `150` is deliberately absent: it announces the count and is followed by `151` responses,
/// not by text of its own.
pub const TEXT_FOLLOWS_CODES: &[u16] = &[110, 111, 112, 113, 114, 151, 152];

/// The 5xx codes `send_dict_error` may send, each with RFC 2229's own text as its default.
pub const ERROR_CODES: &[(u16, &str)] = &[
    (500, "Syntax error, command not recognized"),
    (501, "Syntax error, illegal parameters"),
    (502, "Command not implemented"),
    (503, "Command parameter not implemented"),
    (530, "Access denied"),
    (
        531,
        "Access denied, use \"SHOW INFO\" for server information",
    ),
    (532, "Access denied, unknown mechanism"),
    (
        550,
        "Invalid database, use \"SHOW DB\" for list of databases",
    ),
    (
        551,
        "Invalid strategy, use \"SHOW STRAT\" for a list of strategies",
    ),
    (552, "No match"),
    (554, "No databases present"),
    (555, "No strategies available"),
];

/// The three informational text responses `send_dict_text` renders, with their status text.
pub const TEXT_CODES: &[(u16, &str)] = &[
    (112, "database information follows"),
    (113, "help text follows"),
    (114, "server information follows"),
];

/// The header a MIME-mode text block is prefaced with (RFC 2229 §3.10.1, OPTION MIME).
///
/// RFC 2045's default is `text/plain; charset=us-ascii`, which is false for a definition with
/// an accent in it, so the charset is stated rather than left to the default.
pub const MIME_HEADER: &str =
    "Content-type: text/plain; charset=utf-8\r\nContent-transfer-encoding: 8bit\r\n\r\n";

/// Why a command line could not be split into parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    /// A `"` or `'` was opened and never closed.
    UnterminatedQuote,
    /// The line ended in a lone backslash.
    DanglingEscape,
}

/// Split a command line into parameters per RFC 2229 §2.2.
///
/// Parameters are separated by spaces or tabs. A parameter may be an atom, a double-quoted
/// string, a single-quoted string, or any concatenation of those; a backslash quotes the next
/// character wherever it appears. The real `dict(1)` client quotes every word it sends
/// (`define * "hello"`), so quoting is the common case rather than an edge.
pub fn split_args(line: &str) -> Result<Vec<String>, ArgError> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match (quote, c) {
            (_, '\\') => {
                let next = chars.next().ok_or(ArgError::DanglingEscape)?;
                current.push(next);
                in_token = true;
            }
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                in_token = true;
            }
            (None, ' ') | (None, '\t') => {
                if in_token {
                    args.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            (None, c) => {
                current.push(c);
                in_token = true;
            }
        }
    }
    if quote.is_some() {
        return Err(ArgError::UnterminatedQuote);
    }
    if in_token {
        args.push(current);
    }
    Ok(args)
}

/// Render a string as a DICT double-quoted string: control characters become spaces (a CR or
/// LF would end the status line and forge the next one), and `\` and `"` are escaped.
pub fn quoted(s: &str) -> String {
    let clean = sanitize::line_field(s);
    let mut out = String::with_capacity(clean.len() + 2);
    out.push('"');
    for c in clean.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Render a database or strategy name: bare when it is a plain atom (which every real name
/// is — `wn`, `gcide`, `prefix`), quoted otherwise so a name with a space in it cannot split
/// one listing line into two columns.
pub fn atom(s: &str) -> String {
    let plain = !s.is_empty()
        && sanitize::strip_controls(s) == s
        && s.chars()
            .all(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\'));
    if plain {
        s.to_string()
    } else {
        quoted(s)
    }
}

/// Split one line into pieces of at most [`MAX_TEXT_LINE_CONTENT`] bytes, on char boundaries.
fn wrap_line(line: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = line;
    while rest.len() > MAX_TEXT_LINE_CONTENT {
        let mut cut = MAX_TEXT_LINE_CONTENT;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        pieces.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    pieces.push(rest);
    pieces
}

/// The lines of a model-supplied text body, ready for a text block: line endings normalised,
/// control characters other than tab removed, long lines wrapped, trailing blank lines
/// dropped. Not yet dot-stuffed — [`text_block`] does that, once, for every line it writes.
pub fn text_lines(text: &str) -> Vec<String> {
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    let clean = sanitize::multiline(&normalised);
    let trimmed = clean.trim_end_matches('\n');
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split('\n')
        .flat_map(|line| wrap_line(line).into_iter().map(str::to_string))
        .collect()
}

/// Dot-stuff one text line (RFC 2229 §2.4.1): a line that begins with `.` gets a second one,
/// so the only line that reads as `.` alone is the terminator this module writes.
pub fn stuff(line: &str) -> String {
    if line.starts_with('.') {
        format!(".{line}")
    } else {
        line.to_string()
    }
}

/// A status line followed by a dot-terminated text block.
pub fn text_block(status_line: &str, lines: &[String]) -> String {
    let mut out = String::new();
    out.push_str(status_line);
    out.push_str("\r\n");
    for line in lines {
        out.push_str(&stuff(line));
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

/// One definition, as the model supplies it.
#[derive(Debug, Clone)]
pub struct Definition {
    pub word: String,
    pub database: String,
    pub database_description: String,
    pub text: String,
}

/// `150` + one `151` block per definition + `250`, or `552 no match` when there are none.
pub fn render_definitions(definitions: &[Definition]) -> String {
    if definitions.is_empty() {
        return "552 no match\r\n".to_string();
    }
    let mut out = format!(
        "150 {} definitions retrieved - definitions follow\r\n",
        definitions.len()
    );
    for d in definitions {
        let status = format!(
            "151 {} {} {} - text follows",
            quoted(&d.word),
            atom(&d.database),
            quoted(&d.database_description)
        );
        out.push_str(&text_block(&status, &text_lines(&d.text)));
    }
    out.push_str("250 ok\r\n");
    out
}

/// `152` + one `database "word"` line per match + `250`, or `552 no match`.
pub fn render_matches(matches: &[(String, String)]) -> String {
    if matches.is_empty() {
        return "552 no match\r\n".to_string();
    }
    let lines: Vec<String> = matches
        .iter()
        .map(|(db, word)| format!("{} {}", atom(db), quoted(word)))
        .collect();
    let mut out = text_block(
        &format!("152 {} matches found - text follows", matches.len()),
        &lines,
    );
    out.push_str("250 ok\r\n");
    out
}

/// A `name "description"` listing (`110` databases or `111` strategies) + `250`, or the
/// listing's own empty-set code (`554` / `555`).
pub fn render_listing(
    entries: &[(String, String)],
    code: u16,
    noun: &str,
    empty_line: &str,
) -> String {
    if entries.is_empty() {
        return format!("{empty_line}\r\n");
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|(name, desc)| format!("{} {}", atom(name), quoted(desc)))
        .collect();
    let mut out = text_block(
        &format!("{code} {} {noun} - text follows", entries.len()),
        &lines,
    );
    out.push_str("250 ok\r\n");
    out
}

pub fn render_databases(entries: &[(String, String)]) -> String {
    render_listing(
        entries,
        110,
        "databases present",
        "554 no databases present",
    )
}

pub fn render_strategies(entries: &[(String, String)]) -> String {
    render_listing(
        entries,
        111,
        "strategies available",
        "555 no strategies available",
    )
}

/// `112` / `113` / `114` + a text block + `250`. `None` for any other code.
pub fn render_text(code: u16, text: &str) -> Option<String> {
    let (_, label) = TEXT_CODES.iter().find(|(c, _)| *c == code)?;
    let mut out = text_block(&format!("{code} {label}"), &text_lines(text));
    out.push_str("250 ok\r\n");
    Some(out)
}

/// A single 5xx status line. `None` for a code outside [`ERROR_CODES`].
///
/// An empty message takes RFC 2229's own text for the code. The message is one line, so
/// control characters become spaces, and it is cut to 900 characters so the line stays well
/// inside [`MAX_LINE_BYTES`].
pub fn render_error(code: u16, message: &str) -> Option<String> {
    let (_, default) = ERROR_CODES.iter().find(|(c, _)| *c == code)?;
    let text = sanitize::token(message, 900);
    let text = if text.is_empty() {
        default.to_string()
    } else {
        text
    };
    Some(format!("{code} {text}\r\n"))
}

/// The numeric code a rendered response begins with.
pub fn leading_code(response: &[u8]) -> Option<u16> {
    let head = response.get(..3)?;
    if !head.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(head).ok()?.parse().ok()
}

/// Insert [`MIME_HEADER`] after every status line that opens a text block.
///
/// Applied by the session loop to a connection that has sent `OPTION MIME`. It works on the
/// rendered bytes rather than inside the renderers because the renderers are also the action
/// executor, which has no idea what a given connection negotiated. That is safe only because
/// every response it sees was produced by this module: inside a block the loop consumes lines
/// until the lone `.` terminator, and dot-stuffing guarantees no content line reads as one, so
/// a definition that happens to begin with `151 ` is never mistaken for a status line.
pub fn apply_mime(response: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(response.len() + 128);
    let mut in_block = false;
    for line in response.split_inclusive(|b| *b == b'\n') {
        out.extend_from_slice(line);
        if in_block {
            if line == b".\r\n" || line == b".\n" {
                in_block = false;
            }
            continue;
        }
        if let Some(code) = leading_code(line) {
            if TEXT_FOLLOWS_CODES.contains(&code) {
                out.extend_from_slice(MIME_HEADER.as_bytes());
                in_block = true;
            }
        }
    }
    out
}
