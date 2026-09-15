//! A value substituted into a log template cannot forge a log line.
//!
//! `log_template.rs` is shared infrastructure: every protocol's `log_template` runs through it,
//! so it is the one place that protects all ~140 servers at once — including every protocol
//! nobody has audited. LLDP, CDP and HSRP each fixed control-character injection in their own
//! fields during the quality pass; each of those fixes covered one protocol.
//!
//! The attack is not subtle and that is the point: a log reader, a `grep`, or an NMS ingesting
//! these lines cannot tell a forged line from a real one, which is precisely what an attacker
//! impersonating a switch or a device wants.

use netget::protocol::log_template::{LogLevel, LogTemplate};
use serde_json::json;

fn render(template: &str, data: serde_json::Value) -> String {
    LogTemplate::new()
        .with_info(template)
        .render(LogLevel::Info, &data)
        .expect("the info template must render")
}

/// The headline case: a newline in a model- or wire-supplied field must not become a new line.
#[test]
fn a_newline_in_a_value_cannot_forge_a_second_log_line() {
    let out = render(
        "LLDP neighbour {system_name} on port {port_id}",
        json!({
            "system_name": "core-sw-1\r\nERROR forged line claiming something else",
            "port_id": "Gi0/1",
        }),
    );

    assert!(
        !out.contains('\n') && !out.contains('\r'),
        "one rendered template must be one line: {out:?}"
    );
    assert!(
        out.contains("core-sw-1"),
        "the real part of the value must survive: {out:?}"
    );
}

/// ESC is the one that matters most and the easiest to forget: these lines reach a terminal,
/// where an escape sequence forges a *screen* rather than merely a line — it can clear what a
/// reader already saw, or repaint it as something else.
#[test]
fn escape_sequences_do_not_survive_into_a_log_line() {
    let out = render(
        "CDP advertisement from {device_id}",
        json!({ "device_id": "switch\u{1b}[2J\u{1b}[Hnothing to see here" }),
    );

    assert!(!out.contains('\u{1b}'), "ESC reached a log line: {out:?}");
}

/// Substitution replaces rather than deletes, so two values cannot silently merge into one
/// plausible-looking value. This is the rule `utils::sanitize::line_field` exists to express —
/// deleting would render `Good RegistrarRegistrant`, which reads as a single real value.
#[test]
fn stripping_does_not_merge_the_two_sides_of_the_control_character() {
    let out = render(
        "registrar={registrar}",
        json!({ "registrar": "Good\r\nEvil" }),
    );

    assert!(
        out.contains("Good  Evil") || out.contains("Good Evil"),
        "the two sides must stay separate words rather than merging: {out:?}"
    );
    assert!(
        !out.contains("GoodEvil"),
        "deleting the control characters merged two values into one: {out:?}"
    );
}

/// C1 controls (U+0080..U+009F) and DEL are controls too, and a byte-oriented
/// `is_ascii_control` check misses every one of them. Real non-ASCII text must be untouched.
#[test]
fn c1_controls_go_but_real_text_stays() {
    let out = render(
        "device={name}",
        json!({ "name": "Zürich\u{0085}naïve\u{007f}日本語" }),
    );

    assert!(out.contains("Zürich"), "{out:?}");
    assert!(out.contains("naïve"), "{out:?}");
    assert!(out.contains("日本語"), "{out:?}");
    assert!(!out.contains('\u{0085}'), "C1 NEL survived: {out:?}");
    assert!(!out.contains('\u{007f}'), "DEL survived: {out:?}");
}

/// Nested and function placeholders go through the same substitution point, so none of them is
/// a way round the guard. `{json(.)}` is the interesting one: serde escapes control characters
/// itself, so this asserts the guard does not corrupt already-safe output.
#[test]
fn every_placeholder_form_is_covered() {
    let hostile = "a\r\nb";

    let nested = render(
        "host={headers.host}",
        json!({ "headers": { "host": hostile } }),
    );
    assert!(!nested.contains('\n'), "nested field: {nested:?}");

    let preview = render("body={preview(body,50)}", json!({ "body": hostile }));
    assert!(!preview.contains('\n'), "preview(): {preview:?}");

    // serde already escapes `\r\n` to the two-character sequences `\r` `\n`, so the rendered
    // JSON contains no real control character and must come through unharmed.
    let as_json = render("data={json(.)}", json!({ "body": hostile }));
    assert!(!as_json.contains('\n'), "json(): {as_json:?}");
    assert!(
        as_json.contains(r"\r\n"),
        "json() must keep serde's escaped form rather than losing it: {as_json:?}"
    );
}
