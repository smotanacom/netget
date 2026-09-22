//! Every real-client test must be in the CI job that exists to run real-client tests.
//!
//! # Why this exists
//!
//! `.github/workflows/ci.yml` has a step whose own header says: *"These are the tests every Beta
//! rating that names a binary rests on, and this is the only job with those binaries, so it is
//! the only place they can run."* It drives them by name, in a hand-maintained list.
//!
//! A hand-maintained list of tests is a list that goes stale, and it did. On 22 September 2026,
//! diffing `tests/server/*/real_client_test.rs` against that list found **three** absent, and one
//! of them — `ollama` — was a **Beta** rating whose real-client test the job that exists to run
//! real-client tests did not run. That is precisely the hole the job closes for everyone else.
//!
//! Nobody reading the list would notice: it is thirty-odd filters and the missing one looks like
//! the others by its absence. Diffing it against the filesystem takes a second and is the kind of
//! thing a test should do rather than a person.
//!
//! # Why this is a test and not a CI step
//!
//! It reads two files and needs no build, no features and no binaries, so it holds at the
//! six-protocol CI gate as well as at `--all-features` — the same reason the other whole-tree
//! ratchets read source rather than walking the registry.
//!
//! # What it does not check
//!
//! That the filters *match* anything. A filter naming a test that was renamed would pass here and
//! silently run nothing in CI, because `cargo test <filter>` with no matches exits 0. That is a
//! real second hole and this test does not close it; closing it means CI asserting a non-zero
//! test count per filter, which belongs in the workflow rather than here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Every protocol under `tests/server/` with a `real_client_test.rs`.
fn protocols_with_a_real_client_test() -> BTreeSet<String> {
    let dir = repo_root().join("tests").join("server");
    let mut out = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        panic!("cannot read {}", dir.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("real_client_test.rs").is_file() {
            out.insert(path.file_name().unwrap().to_string_lossy().to_string());
        }
    }
    out
}

#[test]
fn the_scan_finds_real_client_tests() {
    // Without this, an empty scan would make the real assertion below vacuously true — the
    // shape that made `audit_event_action_declarations` exempt the worst case.
    let found = protocols_with_a_real_client_test();
    assert!(
        found.len() >= 10,
        "found only {} real_client_test.rs files, which means the scan is broken rather than \
         that the tree lost them: {found:?}",
        found.len()
    );
}

#[test]
fn every_real_client_test_is_named_in_the_ci_evidence_loop() {
    let workflow = repo_root().join(".github").join("workflows").join("ci.yml");
    let text = std::fs::read_to_string(&workflow)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", workflow.display()));

    let mut missing = Vec::new();
    for protocol in protocols_with_a_real_client_test() {
        if !text.contains(&format!("{protocol}::real_client_test")) {
            missing.push(protocol);
        }
    }

    assert!(
        missing.is_empty(),
        "these protocols have a tests/server/<p>/real_client_test.rs that .github/workflows/ci.yml \
         never runs:\n\n  {}\n\n\
         That job's own header calls itself the only place those tests can run, because it is the \
         only job with the third-party binaries installed. A real-client test outside it is \
         evidence nothing exercises — the same hole as a skip-when-missing gate, wearing different \
         clothes.\n\n\
         Add each to the `for filter in \\` list in the \"real-client evidence\" step, and install \
         whatever binary it drives in the apt/download steps above it.",
        missing.join("\n  ")
    );
}
