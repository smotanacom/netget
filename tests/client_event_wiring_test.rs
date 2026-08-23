//! A ratchet on two defect shapes that have each hit dozens of client protocols at once.
//!
//! `tests/event_action_declarations_test.rs` guards the *declaration* side: an action the
//! model can never see, or an advertised name the executor cannot run. This file guards the
//! two shapes on the other side of the LLM call, neither of which a declaration check can
//! reach because both are properties of the connection loop, not of `actions.rs`:
//!
//! 1. **An event type declared and never emitted.** `http2` and `http3` each declared a
//!    `*_CLIENT_CONNECTED_EVENT` that nothing raised, and nothing ever called their request
//!    code either -- both clients initialised themselves and then did nothing for the rest of
//!    their lives.
//!
//! 2. **A `call_llm_for_client` result discarded.** The model is asked, answers, and is
//!    ignored: `actions: _`, `Ok(_) =>`, or `let _ = call_llm_for_client(...)`. This is the
//!    more damaging of the two, because everything looks wired -- the event fires, the model
//!    is billed for a round-trip, the log shows a reply -- and nothing happens. Found in
//!    openai, ollama, mongodb, dynamodb, couchdb's 409-conflict handler, etcd, saml, amqp,
//!    wireguard and igmp, each independently, before this ratchet existed.
//!
//! This is a source scan, deliberately. Both shapes are invisible to the type system and to
//! any runtime test that does not happen to exercise that exact event, which is why they
//! survived so long.
//!
//! **The two baselines below may only shrink.** Fix a protocol, drop it from the list. A new
//! entry means a new instance of a bug this project has now hit dozens of times, and the
//! build fails. There is no opt-out marker: unlike `EventType::with_no_actions()`, which
//! marks a deliberate choice, there is no legitimate reason to ask the model a question and
//! throw the answer away.

use std::collections::BTreeSet;
use std::path::Path;

/// Clients that declare at least one event type nothing emits.
const NEVER_EMITTED_BASELINE: &[&str] = &[
    "amqp",
    "bluetooth",
    "couchdb",
    "datalink",
    "dc",
    "dynamodb",
    "git",
    "http2",
    "http3",
    "icmp",
    "imap",
    "kubernetes",
    "mqtt",
    "nfc",
    "npm",
    "ntp",
    "pypi",
    "socket_file",
    "syslog",
    "telnet",
    "webrtc",
];

/// Clients that discard at least one `call_llm_for_client` result.
const DISCARDED_BASELINE: &[&str] = &[
    "bitcoin",
    "dc",
    "elasticsearch",
    "grpc",
    "isis",
    "jsonrpc",
    "kubernetes",
    "ntp",
    "oauth2",
    "openapi",
    "s3",
    "saml",
    "stun",
    "tor",
    "webdav",
    "webrtc",
    "whois",
];

fn client_dirs() -> Vec<(String, String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/client");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root).expect("read src/client") {
        let dir = entry.expect("dir entry").path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let actions = std::fs::read_to_string(dir.join("actions.rs")).unwrap_or_default();
        let mut rest = String::new();
        collect_rs(&dir, "actions.rs", &mut rest);
        if rest.is_empty() {
            continue;
        }
        out.push((name, actions, rest));
    }
    out.sort();
    out
}

fn collect_rs(dir: &Path, skip: &str, out: &mut String) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs(&p, skip, out);
        } else if p.extension().is_some_and(|e| e == "rs")
            && p.file_name().is_some_and(|f| f != skip)
        {
            out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
            out.push('\n');
        }
    }
}

/// Event-type constants declared in `actions.rs`, whether `pub static ... LazyLock<EventType>`
/// or `pub const ... &str`. Matching only one form is why an earlier version of this audit
/// reported zero never-emitted events across the whole tree.
fn declared_event_consts(actions: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in actions.lines() {
        let line = line.trim_start();
        let Some(rest) = line
            .strip_prefix("pub static ")
            .or_else(|| line.strip_prefix("pub const "))
        else {
            continue;
        };
        let Some(name) = rest.split(':').next() else {
            continue;
        };
        let name = name.trim();
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

/// Strip `//` line comments before scanning.
///
/// Without this the scan flags exactly the protocols that were FIXED: a good fix documents
/// the bug it removed ("actions used to be discarded here (`actions: _`)"), and a naive
/// substring search then finds that sentence and reports the defect all over again. openai,
/// dynamodb and smtp were each flagged this way on this ratchet's first run.
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn discards_llm_result(rest: &str) -> bool {
    let rest = &strip_comments(rest);
    let mut from = 0;
    while let Some(idx) = rest[from..].find("call_llm_for_client") {
        let at = from + idx;
        let before = &rest[at.saturating_sub(80)..at];
        let window = &rest[at..(at + 900).min(rest.len())];
        if before.contains("let _ =")
            || window.contains("actions: _")
            || window.contains("Ok(_) =>")
            || window.contains("Ok(_result) =>")
        {
            return true;
        }
        from = at + "call_llm_for_client".len();
    }
    false
}

#[test]
fn no_new_client_declares_an_event_nothing_emits() {
    let baseline: BTreeSet<&str> = NEVER_EMITTED_BASELINE.iter().copied().collect();
    let mut offenders = BTreeSet::new();
    for (name, actions, rest) in client_dirs() {
        if declared_event_consts(&actions)
            .iter()
            .any(|c| !rest.contains(c.as_str()))
        {
            offenders.insert(name);
        }
    }
    let found: BTreeSet<&str> = offenders.iter().map(String::as_str).collect();
    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these clients declare an event type nothing emits, and are not in the baseline: {new:?}\n\
         Emit the event, or delete the declaration. Declaring an event the loop never raises \
         means any handler the operator or the model writes for it can never fire."
    );
    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these clients no longer declare an unemitted event -- remove them from \
         NEVER_EMITTED_BASELINE so the ratchet keeps its grip: {fixed:?}"
    );
}

#[test]
fn no_new_client_discards_the_models_answer() {
    let baseline: BTreeSet<&str> = DISCARDED_BASELINE.iter().copied().collect();
    let mut offenders = BTreeSet::new();
    for (name, _actions, rest) in client_dirs() {
        if discards_llm_result(&rest) {
            offenders.insert(name);
        }
    }
    let found: BTreeSet<&str> = offenders.iter().map(String::as_str).collect();
    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these clients call the LLM and throw the answer away, and are not in the baseline: \
         {new:?}\n\
         Execute the returned actions. Everything about this shape looks correct -- the event \
         fires, the round-trip is paid for, the log shows a reply -- and nothing happens."
    );
    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these clients no longer discard the model's answer -- remove them from \
         DISCARDED_BASELINE so the ratchet keeps its grip: {fixed:?}"
    );
}
