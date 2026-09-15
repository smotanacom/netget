//! `send_first` must be refused where it cannot be honoured, never silently ignored.
//!
//! Eight servers declared the parameter, read it, threaded it through `spawn`, and dropped it
//! at the far end. The knob was advertised to the model, plumbed through three hops, and did
//! nothing — and an advertised knob that does nothing is worse than an absent one, because the
//! model has no way to discover that turning it does not work.
//!
//! Deleting the declaration was the other option and is worse: an undeclared key is *refused*
//! at startup, so any existing caller passing `send_first: false` — the value that matches what
//! these servers actually do — would start failing. Declaring it and refusing only `true` keeps
//! the honest call working and makes the dishonest one loud.
//!
//! **This test reads source rather than walking the registry**, and that is a correction to its
//! own first version. Walking the registry made it pass or fail on which features the build
//! happened to compile: at `--features ldap,tcp` none of the protocols it knew about existed,
//! so its coverage guard fired and the build went red for a reason unrelated to the code. That
//! is the same defect this repository has now hit three times (`executable_examples_test`'s
//! `checked > 900`, `event_action_declarations_test`'s non-empty client registry), and the cure
//! is the same: read the tree, not the build.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Protocols that **declare** `send_first` and cannot honour it.
///
/// Declaring is the thing that matters, and the distinction cost a wrong list first time
/// round. `jsonrpc` and `zookeeper` *read* the parameter defensively but declare no startup
/// parameters at all — and an undeclared key is already rejected before `spawn`, so their
/// refusal branch is unreachable and there is no description for them to get wrong. Only a
/// protocol that advertises the knob can lie about it.
///
/// `ldap` joined in September 2026, and how it was missed is the useful part: the sweep that
/// fixed the other five matched a discarded `_send_first` **parameter**, and `ldap` discarded
/// it one layer later in a `let _send_first = …` local. Same defect, different shape,
/// invisible to the pattern that caught its neighbours.
const REFUSERS: &[&str] = &["ipp", "ldap", "mssql", "mysql", "postgresql", "redis"];

/// Does this `actions.rs` advertise `send_first` as a startup parameter?
fn declares_send_first(src: &str) -> bool {
    src.contains("name: \"send_first\"")
}

fn actions_files() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(Path::new("src/server")) else {
        return out;
    };
    for entry in entries.flatten() {
        let actions = entry.path().join("actions.rs");
        if actions.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            out.push((name, actions));
        }
    }
    out.sort();
    out
}

/// A protocol that refuses `send_first` must say so in the description the model reads.
///
/// The model picks parameters from the description alone, so a knob documented as working and
/// refused at runtime is the same defect in a new place. Every one of these previously read
/// "not typically needed for this protocol", which reads as *you may set this and it will
/// work, you just usually would not*.
#[test]
fn a_refusing_protocol_says_so_in_its_parameter_description() {
    let mut checked = 0;
    let mut wrong = Vec::new();

    for (name, path) in actions_files() {
        if !REFUSERS.contains(&name.as_str()) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !declares_send_first(&src) {
            continue; // the declaration was removed; nothing left to mis-describe
        }
        checked += 1;

        let lower = src.to_ascii_lowercase();
        if !(lower.contains("unsupported by this server") || lower.contains("refused if set")) {
            wrong.push(format!("{name}: description does not say it is refused"));
        }
        if lower.contains("not typically needed") {
            wrong.push(format!(
                "{name}: still carries the old 'not typically needed' wording"
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "these declare send_first without documenting that it is refused: {wrong:?}"
    );
    assert_eq!(
        checked,
        REFUSERS.len(),
        "expected to inspect all {} refusers from source, saw {checked}. A protocol directory \
         was renamed or removed, or the declaration is gone — either way this list is stale.",
        REFUSERS.len()
    );
}

/// Declaring the parameter is not enough: `true` has to be refused at runtime.
///
/// A description saying "refused" over code that ignores the value is the same lie one layer
/// out, and it is the easier of the two halves to forget — the description is what a reviewer
/// reads.
#[test]
fn a_refusing_protocol_actually_returns_an_error_for_true() {
    let refusers: BTreeSet<&str> = REFUSERS.iter().copied().collect();
    let mut missing = Vec::new();

    for (name, path) in actions_files() {
        if !refusers.contains(name.as_str()) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !src.contains("send_first is not supported") {
            missing.push(name);
        }
    }

    assert!(
        missing.is_empty(),
        "these document send_first as refused but have no code path that refuses it: \
         {missing:?}\nThe refusal must name the protocol and the reason, so an operator who \
         set it learns why it cannot work."
    );
}

/// No protocol may go back to reading `send_first` and dropping it.
///
/// Both discard shapes are covered, because the second is what hid `ldap` for a month: a
/// `_send_first` parameter in a spawn signature, and a `let _send_first = …` local that
/// consumes the parameter and throws it away.
#[test]
fn no_protocol_reads_send_first_and_discards_it() {
    let mut discarding = Vec::new();

    for (name, path) in actions_files() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Anchored to the start of a line so a COMMENT discussing the pattern does not
        // count. This test caught itself on exactly that: the comment in `ldap/actions.rs`
        // explaining the old `let _send_first = …` was reported as the defect it documents.
        // The same false positive has hit this repository twice before, on `#[cfg(test)]`
        // and on `#[ignore]`.
        if src
            .lines()
            .any(|l| l.trim_start().starts_with("let _send_first"))
        {
            discarding.push(format!("{name}: actions.rs binds `let _send_first`"));
        }
    }

    // The far end of the hop: a spawn that takes the flag and names it away.
    let Ok(entries) = std::fs::read_dir(Path::new("src/server")) else {
        return;
    };
    for entry in entries.flatten() {
        let m = entry.path().join("mod.rs");
        let Ok(src) = std::fs::read_to_string(&m) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        // Only a problem when the protocol also *declares* the parameter — a server that
        // takes the flag from a shared signature and does not advertise it is not lying to
        // anyone.
        // `_send_first: bool` in a spawn signature is FINE when `actions.rs` refuses `true`
        // before ever calling it: the value that arrives is then always `false`, and naming
        // it away says so. It is only a lie when nothing refuses.
        let actions = std::fs::read_to_string(entry.path().join("actions.rs")).unwrap_or_default();
        let declares = declares_send_first(&actions);
        let refuses = actions.contains("send_first is not supported");
        if declares && !refuses && src.contains("_send_first: bool") {
            discarding.push(format!(
                "{name}: mod.rs takes `_send_first` and declares it"
            ));
        }
    }

    assert!(
        discarding.is_empty(),
        "these read send_first and drop it, so the knob is advertised and does nothing: \
         {discarding:?}\nEither honour it, or refuse `true` with a reason as the eight \
         protocols in REFUSERS do."
    );
}
