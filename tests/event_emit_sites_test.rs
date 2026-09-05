//! Every declared event type must have a site that actually raises it.
//!
//! This is the fourth variant in the family the root `CLAUDE.md` catalogues under event/action
//! wiring, and the one no declaration check can see: **an event can be declared, have actions
//! attached, be listed by `get_event_types()`, appear in the protocol's documentation — and
//! never fire.** Declaring actions on it then buys nothing, because the model is never asked.
//! Any handler an operator writes for it can never match either.
//!
//! It has hit this repo repeatedly: the whole USB family (`usb_*_detached`, `usb_msc_read`,
//! `usb_msc_write`, `usb_keyboard_led_status`, every `usb_serial_*`), the BLE profiles, and six
//! clients including `imap`, which advertised `imap_mailbox_selected`,
//! `imap_search_results` and `imap_message_fetched` while nothing raised any of them — so the
//! model got one turn on connect and then went deaf.
//!
//! # What counts as an emit site
//!
//! An event is raised by handing its `EventType` to something **by reference**:
//!
//! ```ignore
//! Event::new(&TCP_DATA_RECEIVED_EVENT, data)          // the common form
//! Event::new(&*crate::server::openapi::actions::X, d) // openapi/openid deref through LazyLock
//! PendingEvent { event_type: &CASSANDRA_QUERY_EVENT }  // cassandra, sip and friends
//! ```
//!
//! So the test looks for `&CONST` in the protocol's own directory, in any spelling. It
//! deliberately does **not** accept `CONST.clone()`: that is what `get_event_types()` does, and
//! counting it would make the check vacuous — declaring the event would prove the event is
//! declared. Getting this wrong in the permissive direction is how a check like this quietly
//! stops working, so the two narrower forms were tried first and both under-reported:
//! `Event::new(&CONST` alone missed the `event_type:` struct-literal form used by cassandra,
//! sip, rtsp, turn, bgp and nine others (52 false positives), and omitting `&*` missed
//! `openapi` and `openid` (2 more).
//!
//! # Why a source scan rather than a registry walk
//!
//! An emit site is a property of the connection loop, not of anything the registry exposes —
//! there is no runtime question to ask. A source scan also covers **all 136 server and 91
//! client protocols at any feature set**, where a registry-walking test only ever sees the
//! protocols compiled into that build (the `registry-audit` job exists precisely because the
//! normal CI set is 6 of 116).
//!
//! **Both baselines are empty and may only grow shorter.** 512 event constants across both
//! trees currently have an emit site; a new entry is a new instance of a bug this project has
//! hit at least three times in bulk.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test event_emit_sites_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Server protocols declaring an `EventType` nothing raises.
const SERVER_NEVER_EMITTED_BASELINE: &[&str] = &[];

/// Client protocols declaring an `EventType` nothing raises.
const CLIENT_NEVER_EMITTED_BASELINE: &[&str] = &[];

fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_rs_recursively(dir: &Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            read_rs_recursively(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push_str(&strip_comments(
                &std::fs::read_to_string(&p).unwrap_or_default(),
            ));
            out.push('\n');
        }
    }
}

/// Every `actions.rs` under `root`, with the protocol name it belongs to.
///
/// Recursive, because the USB and Bluetooth families nest one level
/// (`src/server/usb/fido2/actions.rs`) — and the USB family is exactly where this defect was
/// found, so a non-recursive walk would skip the evidence.
fn protocol_actions_files(root: &Path) -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.file_name().is_some_and(|f| f == "actions.rs") {
                let parent = p.parent().unwrap();
                let name = parent
                    .strip_prefix(root)
                    .unwrap_or(parent)
                    .to_string_lossy()
                    .to_string();
                out.push((name, p.clone()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// `pub static SOME_EVENT: LazyLock<EventType>` names declared in `actions.rs`.
///
/// Restricted to statics whose name contains `EVENT`, which is the convention throughout both
/// trees. `pub const X_EVENT: &str` declarations are deliberately excluded: those are plain id
/// strings, never taken by reference, so the rule below cannot say anything true about them.
fn declared_event_statics(actions_src: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in actions_src.lines() {
        let Some(rest) = line.trim_start().strip_prefix("pub static ") else {
            continue;
        };
        let Some(name) = rest.split(':').next().map(str::trim) else {
            continue;
        };
        if name.contains("EVENT")
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        {
            found.insert(name.to_string());
        }
    }
    found
}

/// Is `name` ever taken by reference in `src`? Accepts `&NAME`, `&*NAME`, and any path prefix.
fn taken_by_reference(src: &str, name: &str) -> bool {
    let bytes = src.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = src[from..].find(name) {
        let at = from + rel;
        from = at + name.len();

        // Whole identifier only: `PEER_EVENT` must not match inside `PEER_EVENT_EXTRA`.
        let after_ok = src[from..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if !after_ok {
            continue;
        }

        // Walk left over a path prefix (`crate::server::x::actions::`), then whitespace and an
        // optional deref `*`, and see whether an `&` precedes it.
        let mut i = at;
        while i > 0 {
            let c = bytes[i - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b':' {
                i -= 1;
            } else {
                break;
            }
        }
        while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'*' || bytes[i - 1] == b'\n') {
            i -= 1;
        }
        if i > 0 && bytes[i - 1] == b'&' {
            return true;
        }
    }
    false
}

fn offenders(root: &Path) -> (BTreeSet<String>, Vec<String>) {
    let mut protocols = BTreeSet::new();
    let mut detail = Vec::new();
    for (name, actions_path) in protocol_actions_files(root) {
        let actions = strip_comments(&std::fs::read_to_string(&actions_path).unwrap_or_default());
        let mut all = String::new();
        read_rs_recursively(actions_path.parent().unwrap(), &mut all);
        for konst in declared_event_statics(&actions) {
            if !taken_by_reference(&all, &konst) {
                protocols.insert(name.clone());
                detail.push(format!("  {name}: {konst}"));
            }
        }
    }
    (protocols, detail)
}

fn assert_ratchet(root: &str, baseline: &[&str], label: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(root);
    let (found_owned, detail) = offenders(&dir);
    let found: BTreeSet<&str> = found_owned.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = baseline.iter().copied().collect();

    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these {label} protocols declare an event type that nothing raises, and are not in the \
         baseline: {new:?}\n{}\n\
         Emit it, or delete the declaration. An event that never fires means the model is never \
         asked, and any handler written for it can never match — while everything about it \
         reads as wired.",
        detail.join("\n")
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these {label} protocols now emit every event they declare — remove them from the \
         baseline so the ratchet keeps its grip: {fixed:?}"
    );
}

#[test]
fn every_server_event_type_has_an_emit_site() {
    assert_ratchet("src/server", SERVER_NEVER_EMITTED_BASELINE, "server");
}

#[test]
fn every_client_event_type_has_an_emit_site() {
    assert_ratchet("src/client", CLIENT_NEVER_EMITTED_BASELINE, "client");
}

/// The scan must actually be looking at something.
///
/// A ratchet whose baselines are both empty is indistinguishable from a ratchet that found no
/// files, parsed nothing, and passed vacuously — which is a real way for a guard like this to
/// rot silently when a directory layout changes.
#[test]
fn the_scan_covers_both_trees() {
    for (root, min_protocols, min_constants) in
        [("src/server", 100usize, 200usize), ("src/client", 80, 150)]
    {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(root);
        let files = protocol_actions_files(&dir);
        assert!(
            files.len() >= min_protocols,
            "{root}: found only {} actions.rs files, expected at least {min_protocols} — the \
             emit-site ratchet may be scanning nothing",
            files.len()
        );
        let constants: usize = files
            .iter()
            .map(|(_, p)| {
                declared_event_statics(&strip_comments(
                    &std::fs::read_to_string(p).unwrap_or_default(),
                ))
                .len()
            })
            .sum();
        assert!(
            constants >= min_constants,
            "{root}: found only {constants} declared event constants, expected at least \
             {min_constants}"
        );
    }
}

/// The reference-detection itself, on the spellings that actually occur.
///
/// Two narrower versions of this rule were written first and both under-reported; these cases
/// are the ones that caught it.
#[test]
fn reference_detection_accepts_every_spelling_in_the_tree() {
    assert!(taken_by_reference(
        "Event::new(&TCP_DATA_EVENT, d)",
        "TCP_DATA_EVENT"
    ));
    assert!(taken_by_reference(
        "let e = Event::new(\n    &*crate::server::openapi::actions::OPENAPI_REQUEST_EVENT,\n)",
        "OPENAPI_REQUEST_EVENT"
    ));
    assert!(taken_by_reference(
        "PendingEvent { event_type: &CASSANDRA_QUERY_EVENT, }",
        "CASSANDRA_QUERY_EVENT"
    ));

    // A declaration is not an emit, and neither is listing it in get_event_types().
    assert!(!taken_by_reference(
        "pub static SIP_INVITE_EVENT: LazyLock<EventType> = ...;\n vec![SIP_INVITE_EVENT.clone()]",
        "SIP_INVITE_EVENT"
    ));
    // Whole-identifier matching: a longer name must not satisfy a shorter one.
    assert!(!taken_by_reference(
        "&PEER_MESSAGE_EVENT_EXTRA",
        "PEER_MESSAGE_EVENT"
    ));
}
