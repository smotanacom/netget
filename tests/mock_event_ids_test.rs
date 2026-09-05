//! Every `.on_event("…")` in the test suite must name an event some protocol actually declares.
//!
//! `tests/helpers/mock_action_names.rs` validates the *actions* a mock rule returns, but only
//! once it can resolve the event id to a protocol. When the id matches nothing it returns an
//! empty catalog and `assert_actions_valid_for_event` skips — deliberately, because that is
//! also what a single-feature build looks like from the inside (`--features tcp` cannot see
//! redis's events, and must not fail because of it).
//!
//! The consequence is that the guard is blind exactly when a mock has drifted furthest: get the
//! event id wrong and nothing is checked at all, including the actions. That is not
//! hypothetical. Three suites rotted this way and each failed for a reason that looked like a
//! protocol bug:
//!
//! * `npm` ran its whole e2e suite on `base_stack: "HTTP"`, so it raised `http_request` and
//!   npm's handler never executed. The mocks matched that id, used `uri` where npm emits
//!   `path`, and returned `send_http_response`, which npm cannot execute — none of it checked.
//! * `rss` and `s3` mocked `send_http_response` against events their protocols do not answer
//!   with it.
//!
//! A single-feature build cannot tell "no protocol declares this" from "that protocol is not
//! compiled here", so this check only means something when the whole registry is loaded. It
//! therefore skips unless the build is a wide one, which is what the `registry-audit` CI job
//! (`--all-features`) and a local `--features all-protocols` run provide.
//!
//! It found a real defect the first time it ran, and not in a test: `smb` never implemented
//! `get_event_types`, so the trait default returned an empty vec and SMB_OPERATION_EVENT was
//! invisible to every registry walk — including `tests/event_action_declarations_test.rs`,
//! which audited none of SMB's events as a result.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Wildcards `EventHandlerConfig` understands; they name no single event by design.
const WILDCARDS: &[&str] = &["*", "all"];

/// Below this many server protocols the registry is too narrow for an unknown id to mean
/// anything, so the check would be reporting the feature set rather than a defect.
const WIDE_BUILD_MIN_PROTOCOLS: usize = 100;

fn declared_event_ids() -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for (_, protocol) in netget::protocol::server_registry::registry().all_protocols() {
        for event in protocol.get_event_types() {
            ids.insert(event.id.to_string());
        }
    }
    for protocol in netget::protocol::client_registry::CLIENT_REGISTRY.get_all() {
        for event in protocol.get_event_types() {
            ids.insert(event.id.to_string());
        }
    }
    ids
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Pull the string literal out of every `on_event("…")` in `source`, with its line number.
fn on_event_ids(source: &str) -> Vec<(usize, String)> {
    const NEEDLE: &str = "on_event(\"";
    let mut found = Vec::new();
    for (lineno, line) in source.lines().enumerate() {
        let mut rest = line;
        while let Some(at) = rest.find(NEEDLE) {
            let after = &rest[at + NEEDLE.len()..];
            match after.find('"') {
                Some(end) => {
                    found.push((lineno + 1, after[..end].to_string()));
                    rest = &after[end..];
                }
                None => break,
            }
        }
    }
    found
}

#[test]
fn every_mocked_event_id_is_declared_by_some_protocol() {
    let declared = declared_event_ids();
    let protocol_count = netget::protocol::server_registry::registry()
        .all_protocols()
        .len();
    if protocol_count < WIDE_BUILD_MIN_PROTOCOLS {
        eprintln!(
            "skipping: only {protocol_count} server protocols compiled, so an unknown event id \
             would just be reporting the feature set. Run with --features all-protocols."
        );
        return;
    }

    let mut sources = Vec::new();
    // Deliberately scoped to tests/server for now.
    //
    // Scans the whole `tests` tree, client suites and shared helpers included.
    //
    // It was scoped to tests/server while ~59 sites in tests/client named events no client
    // declares -- imap mocking `imap_command_received` against a client that raises
    // `imap_connected`, and so on. Those are fixed, so the guard now covers them.
    //
    // Including the helpers matters most: a bad id in tests/helpers is not one protocol's
    // problem, it is silently wrong for every suite that uses the helper. Two lived there
    // (`http_request_received`, `tcp_connection_received`) and neither belonged to any
    // protocol in the tree.
    rust_sources(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .as_path(),
        &mut sources,
    );
    sources.sort();

    let mut unknown: Vec<String> = Vec::new();
    for path in &sources {
        // This file quotes the broken ids in its own documentation.
        if path.ends_with("mock_event_ids_test.rs") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let relative = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(path)
            .display()
            .to_string();
        for (line, id) in on_event_ids(&source) {
            if WILDCARDS.contains(&id.as_str()) || declared.contains(&id) {
                continue;
            }
            unknown.push(format!("  {relative}:{line}  on_event({id:?})"));
        }
    }

    assert!(
        unknown.is_empty(),
        "These mock rules name an event no protocol declares, so they can never match. \
         Worse, `assert_actions_valid_for_event` skips an unresolvable id, so the actions \
         they return are unchecked too — the test may be exercising nothing at all:\n{}\n\n\
         Fix the id, and check the base_stack the test opens: npm's suite raised \
         `http_request` because it started an HTTP server instead of an NPM one.",
        unknown.join("\n")
    );
}
