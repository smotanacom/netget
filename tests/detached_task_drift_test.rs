//! A task spawned inside a server keeps running after the server is stopped.
//!
//! `tokio::spawn` detaches. Dropping the `JoinHandle` it returns does not cancel anything — so a
//! connection handler, a per-request task or a keepalive timer spawned that way outlives the
//! server that created it. `stop_server` aborted the accept loop and released the listening
//! socket, and the instance *looked* stopped: the port was free and it was gone from state, while
//! every connection already open kept reading, kept calling the model and kept answering. There
//! was no way to stop it short of killing the process.
//!
//! The shape was consistent across the tree, and it is the reason this is a build gate rather
//! than a review note: **the accept loop is the handle a protocol remembers to register, and the
//! connection it just accepted is the one it forgets.** A measurement on 15 September 2026 found
//! 301 `tokio::spawn` calls across `src/server/*/mod.rs` against 159 `register_server_task`
//! calls, in 102 protocols.
//!
//! `AppState::spawn_server_task` / `spawn_client_task` spawn and register in one call, so the
//! easy path is the correct one. `tests/stop_server_stops_connections_test.rs` is the behavioural
//! contract, asserted from the peer's side — the only vantage that can tell a live connection
//! from an aborted one. This test is the source ratchet that keeps the next one from appearing.
//!
//! # The rule
//!
//! Over every `.rs` file in `src/{server,client}/*/` and `src/{server,client}/*/*/` — not only
//! `mod.rs`; see [`protocol_source_files`] — a `tokio::spawn(..)` is flagged when **all** of
//! these hold:
//!
//! 1. it is a whole statement — the previous non-whitespace, non-comment character is `;`, `{`
//!    or `}`, and the call is followed by `;`;
//! 2. the enclosing function mentions `server_id` or `client_id` before the spawn, so there is
//!    an owner to register the task with;
//! 3. the spawned body does not itself call `register_server_task` / `register_client_task`.
//!
//! # Why the rule stops there
//!
//! Conditions 1 and 3 are what keep this from being the kind of check people silence by editing
//! the baseline. `startup_param_drift_test.rs` records what the alternative costs: its strict
//! first version flagged 57 parameters, most of them fine, and was abandoned.
//!
//! **A spawn bound to a `let` is deliberately out of scope.** Some are registered a line later;
//! the rest are `.await`ed on the connection's exit path, and only a `JoinHandle` can be awaited
//! — `spawn_server_task` returns an `AbortHandle`. Telling those two apart needs dataflow, and
//! the per-connection writer tasks (`amqp`, `mqtt`, `websocket`, `webrtc`, `webrtc_signaling`,
//! `bgp`) are the awaited kind *and* are already covered: a writer ends when its channel yields
//! `None`, which happens when the registered reader task is aborted and drops the sender. So the
//! rule watches the statement form, which is where every instance of the defect actually was.
//!
//! **A tail expression is out of scope for the same reason.** A helper with
//! `-> JoinHandle<()> { tokio::spawn(..) }` is handing the handle to a caller whose job it is to
//! register it, and the `elasticsearch` client does exactly that.
//!
//! Measured on the tree this ratchet was written against: the rule flagged **136** sites, **135**
//! of which were the defect and were converted. The single false positive is the one baseline
//! entry below — 0.7%.
//!
//! **The baseline may only shrink.** Adding a line to it is not how you pass this test.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp \
//!       --test detached_task_drift_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `<path>:<enclosing fn>` for every detached spawn that has an owner available and no reason.
///
/// One entry, and it is a false positive rather than a defect. SSH's `server_id` is an
/// `Option<ServerId>`: the tracked spawn is taken whenever it is `Some`, and this is the `None`
/// arm — a caller with no server for the task to belong to. The rule sees `server_id` named in
/// the enclosing function and cannot see that it is `None` on this branch, which is the exact
/// price of keeping the rule cheap enough not to produce false positives anywhere else.
///
/// It shrinks when `SshServer::spawn_with_config` stops taking an `Option<ServerId>`.
const DETACHED_TASK_BASELINE: &[&str] = &["src/server/ssh/mod.rs:spawn_with_config"];

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

/// Blank out `//` comments, keeping every byte offset. String literals are honoured so a `//`
/// inside a URL does not swallow the rest of the line.
fn strip_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = b.clone();
    let mut i = 0;
    let mut in_str = false;
    while i + 1 < b.len() {
        let c = b[i];
        if in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            i += 1;
            continue;
        }
        if c == '/' && b[i + 1] == '/' {
            let mut j = i;
            while j < b.len() && b[j] != '\n' {
                out[j] = ' ';
                j += 1;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out.into_iter().collect()
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whole-word `needle` anywhere in `hay`.
fn contains_word(hay: &[char], needle: &str) -> bool {
    let n: Vec<char> = needle.chars().collect();
    if n.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - n.len()).any(|i| {
        hay[i..i + n.len()] == n[..]
            && (i == 0 || !is_ident_char(hay[i - 1]))
            && (i + n.len() >= hay.len() || !is_ident_char(hay[i + n.len()]))
    })
}

/// Index of the matching `)` for the `(` at `open`, or `None`.
fn match_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        break;
                    }
                    i += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `(start, name)` of the innermost function declaration that begins a line before `pos`.
///
/// "Begins a line" is what keeps a `fn` inside a closure type or a doc example from being
/// mistaken for the enclosing item.
fn enclosing_fn(chars: &[char], pos: usize) -> (usize, String) {
    let mut best = (0usize, "<file>".to_string());
    let mut i = 0usize;
    while i + 3 <= chars.len() && i < pos {
        if chars[i] == 'f'
            && chars[i + 1] == 'n'
            && chars.get(i + 2).is_some_and(|c| c.is_whitespace())
            && (i == 0 || !is_ident_char(chars[i - 1]))
        {
            // only when nothing but indentation and the usual modifiers precede it on the line
            let mut k = i;
            while k > 0 && chars[k - 1] != '\n' {
                k -= 1;
            }
            let head: String = chars[k..i].iter().collect();
            let head = head.trim();
            let modifiers_only = head.is_empty()
                || head
                    .split_whitespace()
                    .all(|w| w == "pub" || w == "async" || w == "const" || w.starts_with("pub("));
            if modifiers_only {
                let name: String = chars[i + 3..]
                    .iter()
                    .take_while(|c| is_ident_char(**c))
                    .collect();
                best = (i, name);
            }
        }
        i += 1;
    }
    best
}

/// Every flagged site in one file, as `<enclosing fn>` names.
fn sites(src: &str) -> Vec<String> {
    let stripped = strip_comments(src);
    let chars: Vec<char> = stripped.chars().collect();
    let needle: Vec<char> = "tokio::spawn(".chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] != needle[..] {
            i += 1;
            continue;
        }
        let call = i;
        i += needle.len();

        // 1a. statement position
        let mut j = call;
        while j > 0 && chars[j - 1].is_whitespace() {
            j -= 1;
        }
        if j > 0 && !matches!(chars[j - 1], ';' | '{' | '}') {
            continue;
        }
        // 1b. and a whole statement, not a tail expression handed back to a caller
        let open = call + needle.len() - 1;
        let Some(close) = match_paren(&chars, open) else {
            continue;
        };
        let mut k = close + 1;
        while k < chars.len() && (chars[k] == ' ' || chars[k] == '\t') {
            k += 1;
        }
        if k >= chars.len() || chars[k] != ';' {
            continue;
        }
        // 3. a body that registers itself is already doing the right thing
        let body = &chars[open + 1..close];
        if contains_word(body, "register_server_task")
            || contains_word(body, "register_client_task")
        {
            continue;
        }
        // 2. an owner must be in scope
        let (fn_start, fn_name) = enclosing_fn(&chars, call);
        let head = &chars[fn_start..call];
        if !contains_word(head, "server_id") && !contains_word(head, "client_id") {
            continue;
        }
        out.push(fn_name);
    }
    out
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Every `.rs` file of every protocol directory: `src/{server,client}/<p>/*.rs` and, for the
/// families that nest one level deeper (`usb`, the Bluetooth profiles), `<p>/<q>/*.rs`.
///
/// **Every file, not only `mod.rs`.** Until 26 September 2026 this read `mod.rs` alone, the same
/// blind spot `tcp_server_bounds_ratchet_test.rs` had until it was widened: a protocol that keeps
/// its connection handling in a second file (`http2/h2_server.rs`, `nfs/guard.rs`,
/// `mysql/<helper>.rs`, the USB/IP handlers) was never scanned at all, so a detached
/// per-connection task there passed this gate by living one file over.
///
/// Widening it added no finding and so no baseline entry: every `tokio::spawn` outside a
/// `mod.rs` is bound to a `let` — registered a line later (`nfs/guard.rs`'s connection task,
/// `http2/h2_server.rs`'s accept loop), aborted on the exit path (`nfs/guard.rs`'s downstream
/// relay), or held in `usb/guard.rs`'s `AbortOnDrop`. What the widening buys is the next one: a
/// statement-form spawn added to `h2_server.rs` is flagged by this population and was passed by
/// the `mod.rs`-only one (checked both ways when it was widened).
fn protocol_source_files(root: &Path) -> Vec<PathBuf> {
    fn rust_files_in(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }

    let mut files = Vec::new();
    for tree in ["src/server", "src/client"] {
        let dir = root.join(tree);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            rust_files_in(&p, &mut files);
            // one level deeper, for `src/server/usb/<device>/`
            if let Ok(inner) = std::fs::read_dir(&p) {
                for ie in inner.flatten() {
                    let ip = ie.path();
                    if ip.is_dir() {
                        rust_files_in(&ip, &mut files);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

#[test]
fn the_scan_reads_every_file_of_a_protocol_not_only_its_mod_rs() {
    let files = protocol_source_files(&repo_root());
    for expected in [
        "src/server/http2/h2_server.rs",
        "src/server/nfs/guard.rs",
        "src/server/tcp/mod.rs",
    ] {
        assert!(
            files.iter().any(|f| f.ends_with(expected)),
            "the population must include {expected}; a file the scan never opens cannot be \
             flagged, whatever it spawns"
        );
    }
    assert!(
        files
            .iter()
            .any(|f| f.to_string_lossy().contains("src/server/usb/") && !f.ends_with("mod.rs")),
        "the population must reach into nested families' non-`mod.rs` files too"
    );
}

fn current_findings() -> BTreeSet<String> {
    let root = repo_root();
    let mut found = BTreeSet::new();
    for path in protocol_source_files(&root) {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        for fn_name in sites(&src) {
            found.insert(format!("{rel}:{fn_name}"));
        }
    }
    found
}

#[test]
fn no_protocol_detaches_a_task_it_could_have_registered() {
    let found = current_findings();
    let baseline: BTreeSet<String> = DETACHED_TASK_BASELINE
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    let new: Vec<&String> = found.difference(&baseline).collect();
    assert!(
        new.is_empty(),
        "{} task(s) are spawned detached where the server or client that owns them is in \
         scope:\n  {}\n\n\
         `tokio::spawn` does not cancel when the instance stops — dropping its `JoinHandle` only \
         detaches it. A connection handler spawned that way keeps reading, keeps calling the \
         model and keeps answering after `stop_server` has released the listening socket, and \
         the operator has no way to stop it short of killing the process.\n\n\
         Use `AppState::spawn_server_task(server_id, fut).await` (or `spawn_client_task`), which \
         spawns and registers in one call. `src/server/tcp/mod.rs` is the reference.\n\n\
         The `.await` is not optional: without it the future is constructed and never polled, so \
         the task never runs at all — and that compiles, because an unawaited future is a \
         warning. Build with `-D unused_must_use` to catch it.\n\n\
         If the task genuinely must outlive its instance, say so in \
         `DETACHED_TASK_BASELINE` with the reason. That list may only shrink.",
        new.len(),
        new.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let stale: Vec<&String> = baseline.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "DETACHED_TASK_BASELINE names {} site(s) that no longer exist:\n  {}\n\n\
         Delete them. A baseline that outlives what it excused stops being a record of anything.",
        stale.len(),
        stale
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The rule, checked against the shapes it has to tell apart
// ---------------------------------------------------------------------------

#[test]
fn the_rule_flags_a_detached_per_connection_task() {
    assert_eq!(
        sites(
            "async fn spawn(server_id: ServerId) {\n\
             loop {\n\
             tokio::spawn(async move { handle(stream).await; });\n\
             }\n}"
        ),
        vec!["spawn".to_string()],
        "the plain per-connection spawn inside an accept loop is the whole class"
    );
}

#[test]
fn the_rule_ignores_a_spawn_with_no_owner_in_scope() {
    assert!(
        sites("fn spawn_heartbeat(out_tx: Sender<Vec<u8>>) { tokio::spawn(async move { tick().await; }); }")
            .is_empty(),
        "with neither `server_id` nor `client_id` in the function there is nothing to register \
         the task with, and contorting the code to get one is not this gate's business"
    );
}

#[test]
fn the_rule_ignores_a_bound_handle() {
    assert!(
        sites(
            "async fn run(server_id: ServerId) {\n\
             let writer = tokio::spawn(async move { drain(rx).await; });\n\
             let _ = writer.await;\n}"
        )
        .is_empty(),
        "a bound handle is either registered a line later or awaited on the exit path, and only \
         a `JoinHandle` can be awaited — telling those apart needs dataflow, so the rule stays \
         on the statement form where every instance of the defect actually was"
    );
}

#[test]
fn the_rule_ignores_a_handle_returned_to_the_caller() {
    assert!(
        sites(
            "fn spawn_timers(server_id: ServerId) -> JoinHandle<()> {\n\
             tokio::spawn(async move { tick().await; })\n}"
        )
        .is_empty(),
        "a helper handing back a `JoinHandle` is leaving the registration to its caller; the \
         tail expression has no `;` and must not be flagged"
    );
}

#[test]
fn the_rule_ignores_a_spawn_that_registers_itself() {
    assert!(
        sites(
            "async fn connect(client_id: ClientId) {\n\
             tokio::spawn(async move { registrar.register_client_task(client_id, handle).await; });\n}"
        )
        .is_empty(),
        "the `maven` client spawns purely to perform a registration from a sync context; \
         flagging it would ask for the registration to register itself"
    );
}

#[test]
fn the_rule_sees_through_a_comment_that_mentions_the_call() {
    assert!(
        sites(
            "async fn run(server_id: ServerId) {\n\
             // this used to be a bare tokio::spawn(async move { .. });\n\
             state.spawn_server_task(server_id, async move { go().await; }).await;\n}"
        )
        .is_empty(),
        "prose about the defect is how the other source ratchets in this repo grew false \
         positives; comments are blanked before the scan"
    );
}

#[test]
fn the_rule_names_the_enclosing_function_so_the_key_survives_an_edit() {
    assert_eq!(
        sites(
            "async fn first(server_id: ServerId) { go().await; }\n\
             async fn second(server_id: ServerId) {\n\
             tokio::spawn(async move { go().await; });\n\
             }"
        ),
        vec!["second".to_string()],
        "keying the baseline on a line number would make it stale on the next unrelated edit"
    );
}
