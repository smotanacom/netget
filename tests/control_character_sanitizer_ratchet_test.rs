//! A hand-rolled control-character filter outside `src/utils/` fails the build.
//!
//! **Why this is a ratchet and not a style rule.** Six protocols wrote six different answers
//! to the same question before `src/utils/sanitize.rs` existed — `gopher` mapped tab/CR/LF to
//! a space and let ESC through, `finger`'s server deleted every ASCII control including tab,
//! `finger`'s client kept tab but not CR, `ident` deleted, `whois` had none at all for a long
//! time, and `whois` is the one the others were told to copy. Each was written by reading a
//! neighbour, which is exactly how the disagreement spread.
//!
//! The choice between them is a correctness choice, not a formatting one. Deleting a control
//! character where the format has columns joins the two sides into one word —
//! `Good RegistrarRegistrant` reads as a single legitimate value, and that is its own small
//! lie. Substituting a space where the format has no columns adds content that was not sent.
//! `utils::sanitize` names the four answers (`line_field`, `strip_controls`, `multiline`,
//! `token`) and documents which format each belongs to; the point of this scan is that the
//! next site has to pick one of them rather than invent a seventh.
//!
//! **What is deliberately still hand-rolled.** Every entry in [`ALLOWED`] below. They fall into
//! two kinds and neither is a sanitizer:
//!
//! * **Validators** — where the *model* wrote the string, LLDP, CDP, HSRP, NATS and `git`
//!   refuse a control character rather than rewriting it, because silently rewriting the
//!   model's answer would make the frame disagree with the decision the log records. Stripping
//!   is for text a *peer* sent, which cannot be asked to resend.
//! * **Encoding predicates** — `kafka`, `rawip`, `m3ua`, `radius` and `mssql` ask "is this
//!   faithfully renderable as text?" to decide between a `utf8` and a `hex` event field. They
//!   inspect; they do not filter.
//!
//! The baseline may only shrink. A new occurrence in a file that is not listed fails outright;
//! an extra occurrence in a listed file fails too, so converting a validator into a filter in
//! place still has to come past this comment.

use std::path::{Path, PathBuf};

/// Files under `src/` that may still name a control-character predicate, with the reason.
///
/// The count is the number of occurrences allowed in that file. It may only go down.
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "src/client/nats/actions.rs",
        1,
        "validator: a NATS control line is space-delimited and CRLF-terminated, so check_token \
         rejects rather than strips — stripping would publish on a subject nobody asked for",
    ),
    (
        "src/server/nats/actions.rs",
        1,
        "validator: the server half of the same rule",
    ),
    (
        "src/server/cdp/codec.rs",
        1,
        "validator: check_text_field refuses a control character the model wrote, because the \
         frame must agree with the decision the log records. The neighbour-supplied direction \
         (identifier_text) goes through utils::sanitize::line_field",
    ),
    (
        "src/server/lldp/codec.rs",
        1,
        "validator: reject_control_characters, the LLDP half of the same split — \
         identifier_text_of takes the sanitize path",
    ),
    (
        "src/server/hsrp/codec.rs",
        1,
        "validator: encode_auth_field refuses; decode_auth_field takes the sanitize path",
    ),
    (
        "src/server/git/pack.rs",
        1,
        "validator: a branch name containing whitespace or a control character is one git \
         itself refuses, so the model gets an error rather than a silently different ref",
    ),
    (
        "src/server/kafka/mod.rs",
        1,
        "encoding predicate: decides whether record bytes are shown to the model as utf8 or as \
         hex. It inspects the bytes and rewrites nothing",
    ),
    (
        "src/server/rawip/mod.rs",
        1,
        "encoding predicate: the same utf8-or-hex decision for a raw IP payload",
    ),
    (
        "src/server/m3ua/mod.rs",
        1,
        "encoding predicate: the same utf8-or-hex decision for an M3UA payload",
    ),
    (
        "src/server/radius/mod.rs",
        1,
        "encoding predicate: decides whether a RADIUS attribute is reported as utf8 or hex",
    ),
    (
        "src/server/mssql/mod.rs",
        1,
        "parser: take_while bounding how far SQL text extracted from an RPC body runs. It \
         selects a slice; it does not rewrite one",
    ),
    (
        "src/server/openvpn/packet.rs",
        1,
        "not a character predicate at all: OpenVpnOpcode::is_control asks whether the packet is \
         a control packet rather than a data one. Listed so the scan does not have to guess \
         which receiver a bare `is_control()` is called on",
    ),
];

/// The tokens that name a control-character predicate.
///
/// `is_control()` is matched with its parentheses so `is_control_packet` and friends do not
/// register; `src/server/openvpn/packet.rs` shows that even then the receiver can be something
/// other than a `char`, which is why the allow-list carries a reason per file rather than the
/// scan trying to infer one.
const PREDICATES: &[&str] = &["is_ascii_control", "is_control()"];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_new_hand_rolled_control_character_filter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = root.join("src");

    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();

    let mut unlisted: Vec<String> = Vec::new();
    let mut grew: Vec<String> = Vec::new();

    for file in &files {
        let relative = file
            .strip_prefix(root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");

        // `src/utils/sanitize.rs` is the module this rule points at; the rest of `src/utils`
        // is shared infrastructure that is allowed to reason about characters directly.
        if relative.starts_with("src/utils/") {
            continue;
        }

        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let found: usize = PREDICATES.iter().map(|p| text.matches(p).count()).sum();
        if found == 0 {
            continue;
        }

        match ALLOWED.iter().find(|(path, _, _)| *path == relative) {
            None => unlisted.push(format!(
                "  {relative}: {found} occurrence(s) of a control-character predicate.\n    \
                 Use crate::utils::sanitize — line_field (substitute, for a value in a \
                 structured line), strip_controls (delete), multiline (free multi-line text) \
                 or token (identifier with a length bound). If this really is a validator or \
                 an encoding predicate rather than a filter, add it to ALLOWED with the reason."
            )),
            Some((_, allowed, reason)) if found > *allowed => grew.push(format!(
                "  {relative}: {found} occurrence(s), baseline {allowed}.\n    \
                 The baseline may only shrink. Existing entry: {reason}"
            )),
            Some(_) => {}
        }
    }

    assert!(
        unlisted.is_empty() && grew.is_empty(),
        "hand-rolled control-character handling outside src/utils/:\n\n{}{}",
        unlisted.join("\n"),
        grew.join("\n")
    );
}

#[test]
fn the_allow_list_names_only_files_that_exist_and_still_match() {
    // An allow-list entry for a file that no longer matches is a baseline that failed to
    // shrink when the code did — the next person reads it as permission that is still needed.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stale = Vec::new();

    for (relative, allowed, _) in ALLOWED {
        let path = root.join(relative);
        let Ok(text) = std::fs::read_to_string(&path) else {
            stale.push(format!("  {relative}: listed but not readable"));
            continue;
        };
        let found: usize = PREDICATES.iter().map(|p| text.matches(p).count()).sum();
        if found == 0 {
            stale.push(format!(
                "  {relative}: listed with baseline {allowed}, but nothing matches any more — \
                 remove the entry"
            ));
        }
    }

    assert!(
        stale.is_empty(),
        "stale entries in the control-character allow-list:\n{}",
        stale.join("\n")
    );
}

#[test]
fn the_sanitize_variants_do_what_the_sites_relying_on_them_need() {
    use netget::utils::sanitize;

    // line_field: no forged line, and no merged word either. This is the whois/gopher/cdp/
    // lldp/hsrp/redis/nats-error contract.
    assert_eq!(
        sanitize::line_field("Good Registrar\r\nRegistrant Name: Bar"),
        "Good Registrar  Registrant Name: Bar",
        "a control character must become a space: deleting it merges the two sides into one \
         word that reads as a single legitimate value"
    );
    assert_eq!(
        sanitize::line_field("menu\tfield"),
        "menu field",
        "gopher menu fields are tab-delimited, so a tab inside one would forge a field"
    );
    assert_eq!(
        sanitize::line_field("plain\u{1b}[31mred"),
        "plain [31mred",
        "ESC forges a screen, not merely a line"
    );

    // strip_controls: deletion, where the format has no columns to shift.
    assert_eq!(sanitize::strip_controls("one\r\nquery"), "onequery");

    // multiline: newline and tab survive, because the field is free text where a tab delimits
    // nothing — this is finger's plan/project, on both the server and the client side.
    assert_eq!(
        sanitize::multiline("line one\nindented\tvalue\u{1b}[31m"),
        "line one\nindented\tvalue[31m"
    );
    assert_eq!(
        sanitize::multiline("bare\rcr"),
        "barecr",
        "a lone CR is dropped: a protocol wanting it as a line break normalises before calling"
    );

    // token: strip, trim, then bound by characters rather than bytes.
    assert_eq!(sanitize::token("  ab\u{7f}cd  ", 16), "abcd");
    assert_eq!(
        sanitize::token("ééééé", 3),
        "ééé",
        "the bound is characters — a byte slice here would split a multi-byte character"
    );
}
