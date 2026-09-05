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
const NEVER_EMITTED_BASELINE: &[&str] = &[];

/// Clients that discard at least one `call_llm_for_client` result.
const DISCARDED_BASELINE: &[&str] = &[];

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
    let src = strip_comments(rest);
    // Only unambiguous shapes.
    //
    // An earlier version also flagged `Ok(_) =>` anywhere within 900 characters after a
    // `call_llm_for_client` call. That is far too loose: `Ok(_) => {}` is the correct
    // catch-all arm of a `match protocol.execute_action(..)` that already handles the
    // meaningful variants, and ntp and tor were both flagged for having one while
    // executing the model's actions perfectly well. The window was also too tight in the
    // other direction -- mdns and postgresql discard an answer more than 900 characters
    // from the call and were missed entirely. Whole-file, two literal shapes, no window.
    src.contains("actions: _") || regex_like_let_underscore_call(&src)
}

/// `let _ = ...call_llm_for_client`, tolerating a path prefix and whitespace.
fn regex_like_let_underscore_call(src: &str) -> bool {
    src.match_indices("call_llm_for_client").any(|(idx, _)| {
        let before = &src[idx.saturating_sub(80)..idx];
        let Some(pos) = before.rfind("let _ =") else {
            return false;
        };
        // Nothing but a path between `let _ =` and the call.
        before[pos + "let _ =".len()..]
            .chars()
            .all(|c| c.is_alphanumeric() || c == ':' || c == '_' || c.is_whitespace())
    })
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

// ---------------------------------------------------------------------------------------
// The three shapes the scan above cannot see
// ---------------------------------------------------------------------------------------
//
// `discards_llm_result` looks for two literal spellings: `actions: _` and
// `let _ = ...call_llm_for_client`. Neither of the following is either of those, and all three
// were found in the tree after that ratchet was already green:
//
//   A. `if let Err(e) = call_llm_for_client(..).await { error!(..) }`
//      The success arm does not exist. Nothing is named, so nothing looks discarded — the
//      code reads as "handle the error", and the actions are simply never bound. This was
//      `tor`'s bootstrap-complete handler and is the most common of the three.
//
//   B. `Ok(ClientLlmResult { memory_updates, .. })`
//      The `..` drops the actions. `torrent_tracker` cut every follow-up chain this way: the
//      model was told what the tracker replied, chose what to do next, and was ignored.
//
//   C. `match call_llm_for_client(..).await { Ok(_) => trace!("called successfully"), .. }`
//      `tor`'s connected-event handler. The log line even says it worked.
//
//   D. the binding exists but is only ever `.actions.len()`-ed into a log.
//
// These are found structurally rather than by substring, because the distinguishing feature
// is *where* the pattern sits: an `Ok(_) => {}` catch-all arm on an inner
// `match protocol.execute_action(..)` is correct and extremely common, and an earlier
// substring version of this check flagged `ntp` and `tor` for having one while they executed
// the model's actions perfectly well. So the scan finds the call, balances its parentheses,
// takes the block that consumes the result, and reads the arm patterns at that block's own
// depth — nothing nested.
//
// Validated against the pre-fix sources of `tor` and `torrent_tracker`: it reports exactly
// the three real sites and nothing else across all 91 clients.

/// Clients that ask the model what to do and cannot act on the answer.
///
/// **Empty, and it took eleven protocols to get here.** Every entry that was on this list was
/// removed by fixing the client, not by relaxing the rule. What the sweep produced is one
/// judgement worth keeping: the fix is not always "execute the actions".
///
/// * Where the handle is **shareable** — couchdb's `Arc<Mutex<couch_rs::Client>>`, nfc's
///   `pcsc::Context`, amqp's `Arc<AmqpSession>`, torrent_tracker's HTTP client — the answer is
///   to execute, bounded by a depth limit. Three of those carried a comment asserting the
///   handle was *not* shareable. All three were wrong, and the comment is why nobody checked.
/// * Where the connection is **genuinely gone** — `dc_client_disconnected`,
///   `websocket_client_closed` — the answer is to apply the memory update and *report* the
///   actions that could not be sent. A `..` that silently swallows them is what hid the fact
///   that `dc` was dropping the memory update its own comment promised to keep.
///
/// "An unbounded chain would be bad" is never a reason to discard the answer; it is a reason
/// to bound the chain. `amqp` sat here for exactly that reason.
const ANSWER_DROPPED_BASELINE: &[&str] = &[];

/// Blank the body of `// ...` comments, keeping line structure.
fn without_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => l[..i].to_string(),
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Index just past the delimiter matching the one at `start`. Byte indices; both delimiters
/// are ASCII, so every returned index is a char boundary.
fn balanced(src: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0i32;
    for (i, byte) in b.iter().enumerate().skip(start) {
        if *byte == open {
            depth += 1;
        } else if *byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
    }
    None
}

/// The arm *patterns* of a `match … { … }` block, read at the block's own depth.
///
/// `block` must start at the block's `{`. Reading at depth 0 inside the block is the whole
/// point: it is what separates the success arm of the LLM call from an `Ok(_) => {}` arm on
/// some inner match, which is legitimate and everywhere.
fn match_arm_patterns(block: &str) -> Vec<String> {
    let b = block.as_bytes();
    let mut arms = Vec::new();
    let (mut i, mut depth, mut start) = (1usize, 0i32, 1usize);
    while i < b.len() {
        match b[i] {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            b'=' if depth == 0 && b.get(i + 1) == Some(&b'>') => {
                arms.push(
                    block[start..i]
                        .trim()
                        .trim_start_matches(',')
                        .trim()
                        .to_string(),
                );
                // Skip this arm's body: a braced block, or an expression up to the next
                // top-level comma.
                let mut j = i + 2;
                while j < b.len() && (b[j] == b' ' || b[j] == b'\n') {
                    j += 1;
                }
                if j < b.len() && b[j] == b'{' {
                    i = balanced(block, j, b'{', b'}').unwrap_or(j + 1);
                } else {
                    let mut k = j;
                    let mut d2 = 0i32;
                    while k < b.len() {
                        match b[k] {
                            b'{' | b'(' | b'[' => d2 += 1,
                            b'}' | b')' | b']' => {
                                if d2 == 0 {
                                    break;
                                }
                                d2 -= 1;
                            }
                            b',' if d2 == 0 => break,
                            _ => {}
                        }
                        k += 1;
                    }
                    i = k + 1;
                }
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    arms
}

/// Every `call_llm_for_client` call site in `src` whose result the code cannot act on,
/// as `(line, why)`.
fn answer_dropping_sites(src: &str) -> Vec<(usize, String)> {
    let src = without_comments(src);
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = src[from..].find("call_llm_for_client") {
        let at = from + rel;
        from = at + "call_llm_for_client".len();

        // The call, not the import.
        let line_start = src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if src[line_start..at].trim_start().starts_with("use ") {
            continue;
        }
        let Some(paren) = src[at..].find('(').map(|i| at + i) else {
            continue;
        };
        let Some(after_args) = balanced(&src, paren, b'(', b')') else {
            continue;
        };
        let line = src[..at].matches('\n').count() + 1;

        // What precedes the call in its own statement.
        let stmt = [
            src[..at].rfind(';'),
            src[..at].rfind('{'),
            src[..at].rfind('}'),
        ]
        .into_iter()
        .flatten()
        .max()
        .map(|i| i + 1)
        .unwrap_or(0);
        let head = src[stmt..at].trim();

        // A: no success arm at all.
        if head.starts_with("if let Err") {
            out.push((
                line,
                "`if let Err(..) = call_llm_for_client(..)` — the success arm does not exist, \
                 so every action the model produced is dropped"
                    .to_string(),
            ));
            continue;
        }

        if !head.ends_with("match") {
            continue;
        }
        let tail = &src[after_args..];
        let Some(brace) = tail.find('{') else {
            continue;
        };
        let Some(block_end) = balanced(tail, brace, b'{', b'}') else {
            continue;
        };
        let block = &tail[brace..block_end];

        let Some(ok_arm) = match_arm_patterns(block)
            .into_iter()
            .find(|a| a.starts_with("Ok"))
        else {
            continue;
        };

        // C: `Ok(_) =>`.
        let squashed: String = ok_arm.split_whitespace().collect();
        if squashed == "Ok(_)" {
            out.push((
                line,
                "`Ok(_) =>` — the model's answer is matched and thrown away".to_string(),
            ));
            continue;
        }

        // B: destructured without `actions`.
        if let Some(p) = ok_arm.find("ClientLlmResult") {
            if let Some(open) = ok_arm[p..].find('{').map(|i| p + i) {
                if let Some(close) = balanced(&ok_arm, open, b'{', b'}') {
                    let pat = &ok_arm[open..close];
                    let drops = !pat.contains("actions") || pat.contains("actions: _");
                    if drops {
                        out.push((
                            line,
                            format!(
                                "`Ok(ClientLlmResult {})` — the pattern does not bind `actions`",
                                pat.split_whitespace().collect::<Vec<_>>().join(" ")
                            ),
                        ));
                        continue;
                    }
                }
            }
        }

        // D: bound, but only ever counted into a log.
        if let Some(name) = squashed
            .strip_prefix("Ok(")
            .and_then(|s| s.strip_suffix(')'))
            .filter(|s| {
                !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
            })
        {
            let mut uses = 0usize;
            let mut counted = 0usize;
            let mut scan = 0usize;
            while let Some(rel) = block[scan..].find(name) {
                let idx = scan + rel;
                scan = idx + name.len();
                let before_ok = block[..idx]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_');
                let after = &block[scan..];
                let after_ok = !after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if !(before_ok && after_ok) {
                    continue;
                }
                uses += 1;
                if after
                    .trim_start()
                    .strip_prefix(".actions")
                    .map(|r| r.trim_start().starts_with(".len("))
                    .unwrap_or(false)
                {
                    counted += 1;
                }
            }
            if uses > 0 && uses == counted {
                out.push((
                    line,
                    format!("`{name}` is only ever `.actions.len()`-ed into a log"),
                ));
            }
        }
    }
    out
}

#[test]
fn no_new_client_drops_the_models_answer_structurally() {
    let baseline: BTreeSet<&str> = ANSWER_DROPPED_BASELINE.iter().copied().collect();
    let mut offenders: BTreeSet<String> = BTreeSet::new();
    let mut detail: Vec<String> = Vec::new();

    for (name, actions, rest) in client_dirs() {
        for (src_name, src) in [("actions.rs", &actions), ("(other files)", &rest)] {
            for (line, why) in answer_dropping_sites(src) {
                offenders.insert(name.clone());
                detail.push(format!("  {name}/{src_name}:~{line}: {why}"));
            }
        }
    }

    let found: BTreeSet<&str> = offenders.iter().map(String::as_str).collect();
    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these clients ask the model what to do and cannot act on the answer, and are not in \
         ANSWER_DROPPED_BASELINE: {new:?}\n{}\n\
         Bind the actions and execute them. Where that makes the chain recursive, bound it \
         with a depth limit and an explicitly boxed future — never cut it by staying silent.",
        detail.join("\n")
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these clients no longer drop the model's answer — remove them from \
         ANSWER_DROPPED_BASELINE so the ratchet keeps its grip: {fixed:?}"
    );
}
