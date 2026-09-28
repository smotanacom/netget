//! `request_only` and a peer handle contradict each other, and this holds that from source.
//!
//! `ProtocolMetadataV2::request_only` says a server can only ever *answer* its peers — every
//! message it may send is the reply to one the peer sent first — and the dashboard and the MCP
//! `send_to_peer` tool give its reason instead of "not implemented here yet". A protocol that
//! registers a peer handle (`peer_support::register_peer_channel`) can put an action on the
//! wire to a peer at any moment, which is exactly what `request_only` denies. A protocol doing
//! both would show an enabled `[ send message ]` while its metadata says there is no such
//! thing, so the two may never meet.
//!
//! The scan reads every `.rs` file of each `src/server/<p>/` and `src/server/<p>/<q>/`
//! directory, with `//` comments stripped, so it holds at any feature set — including the CI
//! gate, where a registry walk would see six protocols. It knows both ways metadata is
//! declared: the builder's `.request_only(` and a struct literal's `request_only: Some(`.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp \
//!       --test request_only_declaration_test

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            // A `//` inside a string literal (a URL) is not a comment; the markers this test
            // looks for never follow one on the same line, so cutting there is conservative.
            Some(i) if !line[..i].contains('"') => &line[..i],
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every protocol directory under `src/server`, keyed `p` or `p/q`, with the concatenated
/// comment-stripped source of the `.rs` files directly inside it.
fn protocol_sources() -> BTreeMap<String, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server");
    let mut out = BTreeMap::new();
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&root).expect("src/server") {
        let path = entry.expect("dir entry").path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for sub in std::fs::read_dir(&path).expect("protocol dir") {
            let sub = sub.expect("dir entry").path();
            if sub.is_dir() {
                let sub_name = sub.file_name().unwrap().to_string_lossy().to_string();
                dirs.push((format!("{name}/{sub_name}"), sub));
            }
        }
        dirs.push((name, path));
    }
    for (key, dir) in dirs {
        let mut source = String::new();
        for file in std::fs::read_dir(&dir).expect("protocol dir") {
            let file = file.expect("dir entry").path();
            if file.extension().is_some_and(|e| e == "rs") {
                source.push_str(&strip_line_comments(
                    &std::fs::read_to_string(&file).expect("read source"),
                ));
                source.push('\n');
            }
        }
        out.insert(key, source);
    }
    out
}

fn declares_request_only(source: &str) -> bool {
    source.contains(".request_only(")
        || regex::Regex::new(r"request_only:\s*Some\(")
            .unwrap()
            .is_match(source)
}

fn registers_peer_handle(source: &str) -> bool {
    source.contains("register_peer_channel(")
}

#[test]
fn no_protocol_is_request_only_and_messageable() {
    let sources = protocol_sources();
    let declared: Vec<&String> = sources
        .iter()
        .filter(|(_, s)| declares_request_only(s))
        .map(|(k, _)| k)
        .collect();

    // The scan must see the declarations it exists to check, or it passes vacuously. HTTP is
    // the protocol the declaration was written for, and TCP registers a handle.
    assert!(
        declared.iter().any(|k| k.as_str() == "http"),
        "the scan found no request_only declaration on http; it is not reading what it should. \
         Found: {declared:?}"
    );
    assert!(
        registers_peer_handle(&sources["tcp"]),
        "the scan does not see tcp's peer handle; it is not reading what it should"
    );

    let both: Vec<&String> = declared
        .iter()
        .copied()
        .filter(|k| registers_peer_handle(&sources[k.as_str()]))
        .collect();
    assert!(
        both.is_empty(),
        "these servers declare request_only AND register a peer handle, so the dashboard would \
         offer an enabled [ send message ] that their metadata says cannot exist. Drop the \
         declaration (the handle proves the protocol can message a peer) or the handle: {both:?}"
    );
}
