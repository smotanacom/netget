//! `DevelopmentState::Incomplete` makes `is_available_to_llm()` false, which hides the protocol
//! from every tool list the model ever sees.
//!
//! That is a legitimate state, but it fails *invisibly*: nothing surfaces it, no test breaks,
//! and the protocol simply never appears. The `nfc` client sat at `Incomplete` for months while
//! the root `CLAUDE.md` said none remained — the claim and the code disagreed and there was no
//! mechanism that could have noticed.
//!
//! This reads the source rather than the registries, so it holds at any feature set — including
//! the six-protocol CI gate, where a registry-walking test sees 6 of 116.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Nothing is expected to be hidden. If a protocol genuinely must be, add it here **with the
/// reason in a comment** — an entry is a decision that the model is better off not knowing the
/// protocol exists, which is almost never true. `bluetooth_ble_beacon` is the worked example of
/// the alternative: it is a platform limit, and it declares `Experimental` and returns a clear
/// `Err` from `spawn()` naming the CoreBluetooth key that makes it impossible. Hidden, the model
/// never learns why; refused, the operator gets `ServerStatus::Error` and an explanation.
const DELIBERATELY_HIDDEN: &[&str] = &[];

fn actions_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in ["src/server", "src/client"] {
        collect(Path::new(root), &mut out);
    }
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.file_name().is_some_and(|n| n == "actions.rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_protocol_declares_itself_incomplete() {
    let files = actions_files();
    assert!(
        files.len() > 100,
        "only {} actions.rs files found; this test is not reaching the source tree",
        files.len()
    );

    let allowed: BTreeSet<&str> = DELIBERATELY_HIDDEN.iter().copied().collect();
    let mut hidden = Vec::new();

    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        // Match the fully-qualified form too: roughly half the tree writes
        // `crate::protocol::metadata::DevelopmentState::X`, and a pattern anchored on the bare
        // name silently reports those as declaring nothing.
        if !src.contains("DevelopmentState::Incomplete") {
            continue;
        }
        // Only a `.state(...)` declaration hides a protocol. A `match` arm rendering the
        // variant — which is what every occurrence in `src/docs.rs` and the TUI is — does not.
        if !src.contains(".state(crate::protocol::metadata::DevelopmentState::Incomplete")
            && !src.contains(".state(DevelopmentState::Incomplete")
        {
            continue;
        }
        let name = file
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !allowed.contains(name.as_str()) {
            hidden.push(format!("{name} ({})", file.display()));
        }
    }

    assert!(
        hidden.is_empty(),
        "these protocols declare DevelopmentState::Incomplete, so is_available_to_llm() is \
         false and the model cannot see them at all: {hidden:?}\n\
         That is invisible by construction — no tool list shows them, nothing errors, and the \
         protocol is simply never offered. Prefer declaring the real maturity and returning a \
         clear Err from spawn() naming the limitation, as bluetooth_ble_beacon does. If hiding \
         is genuinely right, add it to DELIBERATELY_HIDDEN with the reason."
    );
}
