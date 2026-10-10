//! Control-character hygiene for text the model supplies.
//!
//! Six protocols hand-rolled a version of this before it lived here — `gopher`'s
//! `sanitize_field`, `finger`'s `strip_controls` and `strip_controls_multiline`, `ident`'s
//! `sanitize_token`, `whois`'s `sanitize_line_field`, and the `finger` client's own copy. They
//! were written by copying a neighbour, which is also how `whois` — the one the others were
//! told to copy — ended up without one at all for a long time.
//!
//! **The four variants below are not interchangeable, and the difference is the whole point.**
//! Picking the wrong one is a silent correctness bug, not a style choice:
//!
//! * [`line_field`] maps a control character to a **space**. Use it for a field that occupies
//!   one line of a structured record — a `Key: value` line, a tab-delimited menu row. Deleting
//!   the character instead would concatenate the two sides into one word
//!   (`Good RegistrarRegistrant`), which reads as a single legitimate value and is its own
//!   small lie. The guarantee is *no forged line*, not that the text is unchanged.
//! * [`strip_controls`] **removes** them. Use it where the surrounding format has no columns to
//!   shift, so a deletion cannot be mistaken for content.
//! * [`multiline`] removes them **except** newline and tab. Use it only for a field whose whole
//!   purpose is to be free multi-line text (finger's `plan` and `project`).
//! * [`token`] is [`strip_controls`], then trim, then a **character** bound — for a short
//!   identifier that also has a length the format cannot exceed.
//!
//! [`json_text`] is not a fifth variant of the same thing: it is for pretty-printed JSON shown
//! as-is, and escapes rather than removes, so the text stays JSON for the same value.
//!
//! All four keep non-ASCII text intact: `char::is_control` is Unicode-aware, so this strips
//! C0, DEL and the C1 range without touching anything a human wrote. ESC goes too — this text
//! reaches a terminal, where an escape sequence is not merely a forged line but a forged
//! *screen*.
//!
//! **An escape sequence goes as a whole, not byte by byte.** Before any control character is
//! filtered, every complete ECMA-48 sequence is taken out with its parameters: a CSI
//! (`ESC [` or U+009B, parameter and intermediate bytes, one final byte), a control string
//! (OSC `ESC ]` / U+009D, DCS `ESC P` / U+0090, SOS `ESC X` / U+0098, PM `ESC ^` / U+009E,
//! APC `ESC _` / U+009F, up to its BEL or ST), and a short `ESC` + final-byte sequence.
//! Removing only the introducer would leave the rest painted as text — `\x1b[2mdim\x1b[0m`
//! reading `[2mdim[0m` — so deliberate styling and a peer's attack both disappear cleanly.
//! [`line_field`] puts a space for every character the sequence occupied, for the same reason
//! it substitutes a lone control character, so it still never changes a value's character
//! count; the others remove it.
//!
//! A control string is removed only when it is terminated, and its body may hold only
//! printable ASCII. A dangling `ESC ]` therefore loses just its introducer rather than
//! swallowing the rest of the line — the text the operator needs to see after a peer's field
//! (`decision=model_reject`) stays visible. A terminated one is removed whole, which is what a
//! terminal would have done with it, so what is left is what the terminal would have shown,
//! without the effect.
//!
//! **A validator is not a sanitizer, and several protocols correctly have one instead.** Where
//! the *model* wrote the string and can be told, LLDP, CDP, HSRP and NATS reject a control
//! character rather than rewriting it: silently rewriting the answer would make the frame
//! disagree with the decision the log records. Stripping is for text a *peer* sent, which
//! cannot be asked to resend. Those sites stay hand-rolled on purpose and are listed, with the
//! reason, in `tests/control_character_sanitizer_ratchet_test.rs`.

/// Whether `s` holds a control character, for a **validator**: a site that refuses such a
/// value (the model's answer, or a peer's field it will not echo) instead of rewriting it.
/// Refusing is right where silently rewriting would make the wire disagree with what was
/// decided; everywhere a value must be kept, use one of the rewriting helpers below.
pub fn has_controls(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// Replace every escape sequence and every control character with spaces, one per character,
/// for a value that occupies one line of a structured record. See the module docs for why this
/// substitutes rather than deletes.
pub fn line_field(s: &str) -> String {
    replace_escape_sequences(s, Some(' '))
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Remove every escape sequence and every control character.
pub fn strip_controls(s: &str) -> String {
    replace_escape_sequences(s, None)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// Remove every escape sequence, and every control character except `\n` and `\t`, for a
/// genuinely multi-line field.
///
/// `\r` is dropped rather than kept: a protocol that wants CRLF endings should normalise them
/// itself *before* calling this — `s.replace("\r\n", "\n").replace('\r', "\n")` — so a lone CR
/// in the model's output becomes the line break it meant rather than surviving as a bare
/// carriage return that overwrites the line a reader has already seen.
///
/// `\t` is kept because this variant is for **free text**, where a tab delimits nothing and
/// cannot forge a record: deleting it is the same column-merging lie [`line_field`] exists to
/// avoid, and a finger `.plan` is aligned with tabs as a matter of course. A format where the
/// tab *is* a delimiter — a gopher menu row — is a [`line_field`], not this.
pub fn multiline(s: &str) -> String {
    replace_escape_sequences(s, None)
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

/// [`strip_controls`], then truncate to `max_chars` **characters** — never bytes, so this
/// cannot split a multi-byte character the way `&s[..n]` does (see `crate::utils::truncate`
/// for the bug that taught us that) — then trim.
///
/// The trim comes **after** the cut, and that order is the whole point: cutting first can land
/// the boundary just after a space, and a value ending in a space is one a second call would
/// shorten again. `token(token(x, n), n) == token(x, n)` for every `x` and `n`, so "already
/// sanitised" is a stable predicate — which matters because a value normalised at ingest and
/// normalised again at render has to compare equal to itself.
pub fn token(raw: &str, max_chars: usize) -> String {
    let cleaned = strip_controls(raw);
    let cut: String = cleaned.trim_start().chars().take(max_chars).collect();
    cut.trim_end().to_string()
}

/// Make pretty-printed JSON safe to paint. `serde_json` escapes U+0000..U+001F inside a string
/// but writes DEL and the C1 range (U+009B is an 8-bit CSI) as raw characters; this writes each
/// of those as the `\uXXXX` escape it could equally have used. Outside a string the printer
/// emits no control character but `\n`, so every one this finds is inside a string, the
/// output is still valid JSON for the same value, and the operator sees which character the
/// peer sent instead of a deletion.
pub fn json_text(pretty: &str) -> String {
    let mut out = String::with_capacity(pretty.len());
    for c in pretty.chars() {
        if c.is_control() && c != '\n' {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `s` with every character of every complete escape sequence replaced by `with` (or the
/// sequence removed, for `None`).
fn replace_escape_sequences(s: &str, with: Option<char>) -> String {
    if !s.chars().any(is_sequence_introducer) {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        match sequence_len(&chars[i..]) {
            Some(len) => {
                if let Some(r) = with {
                    out.extend(std::iter::repeat_n(r, len));
                }
                i += len;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

fn is_sequence_introducer(c: char) -> bool {
    matches!(
        c,
        '\u{1b}' | '\u{9b}' | '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}'
    )
}

/// Length in characters of the escape sequence starting at `rest[0]`, or `None` when nothing
/// recognisable starts there (a lone ESC, an ordinary character).
fn sequence_len(rest: &[char]) -> Option<usize> {
    enum Kind {
        Csi,
        ControlString,
    }
    let (kind, body) = match *rest.first()? {
        '\u{1b}' => match *rest.get(1)? {
            '[' => (Kind::Csi, 2),
            ']' | 'P' | 'X' | '^' | '_' => (Kind::ControlString, 2),
            _ => return short_escape_len(rest),
        },
        '\u{9b}' => (Kind::Csi, 1),
        '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => (Kind::ControlString, 1),
        _ => return None,
    };
    let in_range = |j: usize, lo: u32, hi: u32| {
        rest.get(j)
            .is_some_and(|&c| (lo..=hi).contains(&(c as u32)))
    };
    match kind {
        Kind::Csi => {
            let mut j = body;
            while in_range(j, 0x30, 0x3f) {
                j += 1;
            }
            while in_range(j, 0x20, 0x2f) {
                j += 1;
            }
            if in_range(j, 0x40, 0x7e) {
                Some(j + 1)
            } else {
                // Malformed or cut short: drop what was consumed, keep what follows.
                Some(j)
            }
        }
        Kind::ControlString => {
            let mut j = body;
            loop {
                match rest.get(j).copied() {
                    Some('\u{7}') | Some('\u{9c}') => return Some(j + 1),
                    Some('\u{1b}') if rest.get(j + 1) == Some(&'\\') => return Some(j + 2),
                    Some(c) if (' '..='~').contains(&c) => j += 1,
                    // Unterminated: only the introducer goes, so a dangling one cannot hide
                    // the rest of the line.
                    _ => return Some(body),
                }
            }
        }
    }
}

/// `ESC`, any intermediate bytes (0x20..=0x2F), then one final byte (0x30..=0x7E) — a charset
/// designation, a cursor save, a full reset (`ESC c`).
fn short_escape_len(rest: &[char]) -> Option<usize> {
    let mut j = 1;
    while rest
        .get(j)
        .is_some_and(|&c| (0x20..=0x2f).contains(&(c as u32)))
    {
        j += 1;
    }
    match rest.get(j) {
        Some(&c) if (0x30..=0x7e).contains(&(c as u32)) => Some(j + 1),
        _ => None,
    }
}
