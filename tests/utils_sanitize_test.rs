//! `crate::utils::sanitize` — the three variants are not interchangeable, and these tests are
//! about the difference rather than about the stripping.
//!
//! Six protocols hand-rolled a version of this before it was shared, which is how they came to
//! disagree. The rules worth pinning are the ones a copier gets wrong.

use netget::utils::sanitize;

/// A single-line field SUBSTITUTES rather than deletes. Deleting joins the two sides into one
/// word, which reads as a single legitimate value — the forgery is gone but a new lie is in its
/// place. This is the whole reason `line_field` and `strip_controls` are separate functions.
#[test]
fn a_line_field_substitutes_so_the_two_sides_do_not_merge() {
    let forged = "Good Registrar\r\nRegistrant Name: Attacker";
    let cleaned = sanitize::line_field(forged);

    assert!(
        !cleaned.contains('\r') && !cleaned.contains('\n'),
        "no forged line may survive: {cleaned:?}"
    );
    assert!(
        cleaned.contains("Registrar  Registrant"),
        "the two sides must stay separate words, got {cleaned:?}"
    );
    assert!(
        !cleaned.contains("RegistrarRegistrant"),
        "deleting the control characters merged two values into one: {cleaned:?}"
    );
}

/// ESC is the one that matters most and is the easiest to forget: this text reaches a terminal,
/// where an escape sequence forges a screen rather than merely a line.
#[test]
fn escape_sequences_do_not_survive_any_variant() {
    let attack = "safe\x1b[2J\x1b[Hcleared your screen";

    for (name, out) in [
        ("line_field", sanitize::line_field(attack)),
        ("strip_controls", sanitize::strip_controls(attack)),
        ("multiline", sanitize::multiline(attack)),
        ("token", sanitize::token(attack, 200)),
    ] {
        assert!(!out.contains('\x1b'), "{name} let ESC through: {out:?}");
    }
}

/// C1 controls (U+0080..U+009F) are controls too, and a byte-oriented `is_ascii_control` check
/// misses every one of them. `char::is_control` is Unicode-aware, which is why it is used.
#[test]
fn c1_controls_and_del_go_too_but_real_text_stays() {
    let mixed = "Zürich\u{0085}naïve\u{007f}日本語";
    let cleaned = sanitize::strip_controls(mixed);

    assert_eq!(cleaned, "Zürichnaïve日本語");
}

/// `multiline` exists for fields whose purpose is to be multi-line. It keeps `\n` and drops
/// `\r`: a lone CR is a carriage return that overwrites the line a reader has already seen,
/// so it is not "a newline the field is allowed to have".
#[test]
fn multiline_keeps_newlines_but_not_carriage_returns() {
    let plan = "line one\r\nline two\rOVERWRITTEN\x07";
    let cleaned = sanitize::multiline(plan);

    assert!(cleaned.contains("line one\nline two"), "got {cleaned:?}");
    assert!(!cleaned.contains('\r'), "bare CR survived: {cleaned:?}");
    assert!(!cleaned.contains('\x07'), "BEL survived: {cleaned:?}");
}

/// Truncation counts CHARACTERS. Counting bytes and slicing is the panic this repo already had
/// once, in `src/protocol/log_template.rs` — `&s[..n]` on a multi-byte boundary.
#[test]
fn token_truncates_by_character_and_cannot_split_one() {
    // Ten characters, thirty bytes.
    let wide = "日本語日本語日本語日";
    assert_eq!(wide.len(), 30);

    let cut = sanitize::token(wide, 4);
    assert_eq!(cut.chars().count(), 4);
    assert_eq!(cut, "日本語日");
}

/// A control character that is removed rather than replaced must not leave the surrounding
/// whitespace as the only thing separating two values — `token` trims, so a name that is
/// entirely control characters becomes empty rather than a run of spaces that looks like a name.
#[test]
fn a_token_of_pure_control_characters_becomes_empty() {
    assert_eq!(sanitize::token("\r\n\t\x00", 32), "");
}
