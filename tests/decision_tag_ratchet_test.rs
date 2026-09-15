//! Every server with an LLM path must be able to say **which** way it failed.
//!
//! `grep decision=fail_closed` is the one diagnostic this repository teaches, and it is the
//! only one that works when the wire cannot carry the distinction. Without a `decision=` tag
//! in the log, these three are the same line:
//!
//! - the backend was unreachable or its retries were exhausted (`fail_closed_llm_error`),
//! - the model was asked and deliberately answered with nothing (`model_silent`),
//! - the model explicitly refused, and the peer was correctly rejected (`model_reject`).
//!
//! An operator reading `netget.log` after an incident cannot tell an outage from a policy
//! decision, and for the ~20 protocols on the root `CLAUDE.md`'s deliberately-silent list the
//! wire carries *nothing at all*, so the log is the only place the distinction can live. That
//! is why the silent protocols need the tag most, not least.
//!
//! `src/server/radius/` is the reference: it distinguishes `decision=model_reject` (the model
//! said no), `decision=model_silent` (the model answered with nothing) and
//! `decision=fail_closed_llm_error` (the backend failed), and the fail-closed paths are loud.
//!
//! # The rule
//!
//! Any `src/server/<p>/mod.rs` that calls `call_llm` or builds an `llm_client` must have
//! `decision=` **somewhere in that protocol's own directory**, outside a comment.
//!
//! The tag need not be in `mod.rs` — several protocols decide the outcome in `actions.rs`, and
//! the nested families (`usb/*`, `bluetooth_ble_*`) spread it over submodules. What matters is
//! that the protocol emits one somewhere it owns.
//!
//! # Why a source scan rather than a registry walk
//!
//! The same reason as `event_emit_sites_test`: a registry-walking test only sees the protocols
//! compiled into that build, and the blocking CI job compiles 6 of 116. A source scan holds at
//! every feature set, including the six-protocol gate.
//!
//! # Why comments do not count
//!
//! A doc comment explaining the convention is not a log line. Counting one would make the
//! ratchet vacuous for exactly the protocols most likely to have been half-done — someone
//! writes the prose, does not wire the emit, and the check goes green. So `//` comments are
//! stripped before the search, taking care not to eat a `//` inside a string literal (a
//! `decision=` tag on the same line as a `http://…` URL is real code, and a naive stripper
//! would drop it and report a false offender).
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test decision_tag_ratchet_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Server protocols with an LLM path and no `decision=` tag.
///
/// **Shrink-only.** An entry here is a protocol whose log cannot distinguish a backend outage
/// from a model that chose silence. Adding one is not an option; removing one is the work.
///
/// `tor_relay` is the last entry and was excluded from the sweep that emptied the rest of this
/// list because another change owned that file at the same time.
const NO_DECISION_TAG_BASELINE: &[&str] = &["tor_relay"];

/// Strip `//` comments, leaving `//` that occurs inside a string literal alone.
///
/// Quote parity is enough here: a line with an even number of unescaped `"` before a `//` is
/// outside a string. This is deliberately simpler than a real lexer — it only has to avoid the
/// one false positive that matters, which is truncating a log line at an embedded URL.
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|line| {
            let bytes = line.as_bytes();
            let mut in_string = false;
            let mut i = 0usize;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' if in_string => i += 1,
                    b'"' => in_string = !in_string,
                    b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => {
                        return &line[..i];
                    }
                    _ => {}
                }
                i += 1;
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every protocol directory under `root` that has its own `mod.rs`.
///
/// Recursive one level, because `usb/serial`, `usb/fido2` and friends nest — and `usb` itself
/// also has a `mod.rs`, so both the parent and the children are reported and each is judged on
/// its own source. `src/server/mod.rs` is the crate's own module list, not a protocol, and is
/// skipped.
fn protocol_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            if p.join("mod.rs").is_file() {
                out.push((
                    p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into(),
                    p.clone(),
                ));
            }
            walk(&p, root, out);
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Concatenate every `.rs` file directly in `dir` (not in subdirectories).
///
/// Not recursive: a nested protocol owns its own subdirectory and is judged separately, so
/// recursing would let `usb/mod.rs`'s tag stand in for `usb/serial/`'s missing one.
fn own_sources(dir: &Path) -> String {
    let mut out = String::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "rs"))
        .collect();
    paths.sort();
    for p in paths {
        out.push_str(&strip_comments(
            &std::fs::read_to_string(&p).unwrap_or_default(),
        ));
        out.push('\n');
    }
    out
}

/// Does this protocol ask a model anything?
///
/// `call_llm` covers `action_helper::call_llm` and `call_llm_for_client`; `llm_client` covers
/// the protocols that thread an `OllamaClient` through to a helper of their own. This is the
/// same predicate `PROTOCOL_QUALITY.md` used to measure the starting point (140 servers).
fn has_llm_path(mod_src: &str) -> bool {
    mod_src.contains("call_llm") || mod_src.contains("llm_client")
}

fn offenders(root: &Path) -> (BTreeSet<String>, usize) {
    let mut missing = BTreeSet::new();
    let mut with_llm = 0usize;
    for (name, dir) in protocol_dirs(root) {
        let mod_src =
            strip_comments(&std::fs::read_to_string(dir.join("mod.rs")).unwrap_or_default());
        if !has_llm_path(&mod_src) {
            continue;
        }
        with_llm += 1;
        if !own_sources(&dir).contains("decision=") {
            missing.insert(name);
        }
    }
    (missing, with_llm)
}

#[test]
fn every_server_with_an_llm_path_logs_a_decision_tag() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server");
    let (found_owned, _) = offenders(&root);
    let found: BTreeSet<&str> = found_owned.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = NO_DECISION_TAG_BASELINE.iter().copied().collect();

    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these server protocols call the model and never log a `decision=` tag, and are not in \
         the baseline: {new:?}\n\n\
         Without the tag a backend outage and a deliberate model refusal are the same log line, \
         and `grep decision=fail_closed` — the one diagnostic this repo teaches — finds nothing. \
         Tag every terminal outcome: `model_answer`, `model_silent`, `model_reject`, \
         `fail_closed_llm_error`. See `src/server/radius/` for the worked example, and the \
         Failure behaviour table in any swept protocol's CLAUDE.md for the shape."
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these protocols now log a `decision=` tag — remove them from \
         NO_DECISION_TAG_BASELINE so the ratchet keeps its grip: {fixed:?}"
    );
}

/// The scan must actually be looking at something.
///
/// A ratchet with a near-empty baseline is indistinguishable from one that found no files,
/// parsed nothing and passed vacuously — which is how a guard like this rots silently when a
/// directory layout changes. `PROTOCOL_QUALITY.md` measured 140 servers with an LLM path on
/// 15 September 2026; the floor is set well below that so ordinary drift does not trip it.
#[test]
fn the_scan_covers_the_server_tree() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server");
    let dirs = protocol_dirs(&root);
    assert!(
        dirs.len() >= 120,
        "found only {} server protocol directories, expected at least 120 — the decision-tag \
         ratchet may be scanning nothing",
        dirs.len()
    );
    let (_, with_llm) = offenders(&root);
    assert!(
        with_llm >= 120,
        "found only {with_llm} servers with an LLM path, expected at least 120 — the \
         `call_llm`/`llm_client` predicate may have stopped matching"
    );
}

/// The comment stripper has to get both directions right, and both have bitten before.
#[test]
fn comment_stripping_ignores_prose_but_keeps_code() {
    // A doc comment describing the convention is not an emit site.
    assert!(
        !strip_comments("/// logs decision=model_reject when the model says no")
            .contains("decision=")
    );
    assert!(
        !strip_comments("    // TODO: add decision=fail_closed_llm_error").contains("decision=")
    );

    // A real log line survives, including one carrying a URL whose `//` a naive stripper
    // would mistake for a comment and truncate at — dropping the tag and inventing an offender.
    assert!(strip_comments(r#"log.info(format!("decision=model_answer"));"#).contains("decision="));
    assert!(
        strip_comments(r#"log.error(format!("backend http://x decision=fail_closed_llm_error"));"#)
            .contains("decision="),
        "a `//` inside a string literal is not a comment"
    );

    // A trailing comment is still stripped when it really is one.
    assert!(!strip_comments(r#"let x = "ok"; // decision=model_silent"#).contains("decision="));
}

/// The reference implementation must pass its own rule.
///
/// If `radius` ever stops emitting a tag, every message in this file pointing at it as the
/// worked example is wrong, and the rule has lost the thing it is modelled on.
///
/// Note the shape: radius renders `decision={}` from `Decision::as_str()`, so the *joined*
/// string `decision=model_reject` never appears in its source. The tag and the vocabulary are
/// asserted separately for that reason — and any protocol that builds its tag the same way
/// still satisfies the ratchet, which only looks for `decision=`.
#[test]
fn the_reference_protocol_is_tagged() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/radius");
    let src = own_sources(&dir);
    assert!(
        src.contains("decision="),
        "src/server/radius is this rule's worked example and no longer emits a decision tag"
    );
    for token in [
        "model_reject",
        "fail_closed_no_action",
        "fail_closed_llm_error",
    ] {
        assert!(
            src.contains(token),
            "src/server/radius no longer distinguishes `{token}` — the vocabulary this sweep \
             copied has moved, and the swept protocols now cite a reference that disagrees"
        );
    }
}
