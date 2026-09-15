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
//! All four keep non-ASCII text intact: `char::is_control` is Unicode-aware, so this strips
//! C0, DEL and the C1 range without touching anything a human wrote. ESC goes too — this text
//! reaches a terminal, where an escape sequence is not merely a forged line but a forged
//! *screen*.
//!
//! **A validator is not a sanitizer, and several protocols correctly have one instead.** Where
//! the *model* wrote the string and can be told, LLDP, CDP, HSRP and NATS reject a control
//! character rather than rewriting it: silently rewriting the answer would make the frame
//! disagree with the decision the log records. Stripping is for text a *peer* sent, which
//! cannot be asked to resend. Those sites stay hand-rolled on purpose and are listed, with the
//! reason, in `tests/control_character_sanitizer_ratchet_test.rs`.

/// Replace every control character with a space, for a value that occupies one line of a
/// structured record. See the module docs for why this substitutes rather than deletes.
pub fn line_field(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Remove every control character.
pub fn strip_controls(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Remove every control character except `\n` and `\t`, for a genuinely multi-line field.
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
    s.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

/// [`strip_controls`], then trim, then truncate to `max_chars` **characters** — never bytes, so
/// this cannot split a multi-byte character the way `&s[..n]` does (see `crate::utils::truncate`
/// for the bug that taught us that).
pub fn token(raw: &str, max_chars: usize) -> String {
    let cleaned = strip_controls(raw);
    let trimmed = cleaned.trim();
    trimmed.chars().take(max_chars).collect()
}
