//! A model-supplied code narrowed with `as u16` is fail-open by arithmetic.
//!
//! `status_code: 65736` is not a number any HTTP server should accept, but `65736 as u16` is
//! **200**. The model wrote an explicit refusal; the peer reads a success. Nothing logs, nothing
//! errors, and the cast that did it looks like the most innocuous line in the function.
//!
//! Twelve protocols had this shape and were swept by hand. The two worst show why the class is
//! worth a build failure rather than a review note:
//!
//! * **`ldap`** narrowed `result_code` with `as u8`. `256 → 0`, and LDAP resultCode **0 is
//!   `success`** — so every refusal a multiple of 256 away became the directory reporting that
//!   the bind, write or search completed. The encoder produced a well-formed success.
//! * **`zookeeper`** narrowed `error_code` with `as i32`. `4294967296 → 0` flips `is_error`
//!   false. It defeated an existing `required: true` guard, because that guard checks the
//!   *shape* of the value and `4294967296` is a perfectly valid integer.
//!
//! And `stun` shows that a range check is not automatically a fix: it checked `300..700`
//! **after** the cast, and `65836 as u16` is `300`, so a wrapped value passed the check and was
//! then indistinguishable from a model that legitimately asked for 300. **A range check only
//! works while the original value is still visible.** That is the property this test encodes.
//!
//! # The rule
//!
//! Over `src/{server,client}/*/actions.rs`, a narrowing cast (`as u8`/`u16`/`i16`/`i32`/`u32`)
//! is flagged when **all** of these hold:
//!
//! 1. the expression being cast traces back to `<something>.get("<key>")` or
//!    `<something>["<key>"]` — either in the same expression, through a `.map(|v| v as u16)`
//!    closure, or through one `let` binding;
//! 2. `<key>` has a `status` / `code` / `port` / `id` / `severity` / `count` segment;
//! 3. nothing bounds the value **before** the narrowing — neither a saturating combinator in
//!    the chain (`.min`, `.clamp`, `.filter`, `try_into`, `try_from`, `checked_*`) nor, for the
//!    binding form, a comparison or `contains` naming that same binding between its assignment
//!    and the cast.
//!
//! # Anchoring on the `action.get` chain is the whole design
//!
//! `packet.len() as u16`, checksum folding, and the BER/IPP/STUN length encoders are everywhere
//! and entirely legitimate. A rule that flagged narrowing casts as such would report hundreds of
//! them, and `startup_param_drift_test.rs` documents what happens next: its strict first version
//! flagged 57 parameters, most of them fine, and was abandoned — a false positive in a
//! build-failing check trains people to edit the baseline instead of the code. So the anchor is
//! not the cast. It is **where the number came from**: a value the model typed.
//!
//! The exemption had to be narrow for the mirror-image reason. An early attempt exempted "the
//! enclosing function contains a comparison and mentions the key", which exempted essentially
//! everything, because almost any function body contains a `<` or a `>`. What is checked instead
//! is that the **same binding**, still at its original width, is the thing compared.
//!
//! **The baseline may only shrink.** Refuse the out-of-range value — do not clamp it; 255 is not
//! what the model asked for either, and the error message is what the repair loop reads.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test narrowing_cast_drift_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `role:protocol:file_line:key` for every model-supplied code narrowed without a prior check.
///
/// **Two entries, both real defects, both in a directory this pass was not allowed to touch.**
/// They are not exempt and not disputed — `src/server/torrent_dht/` was being edited by another
/// agent while this ratchet was written, and editing it concurrently is how `master` gets a
/// half-landed change. The fix is the same one the other twelve protocols took:
///
/// `send_find_node_response` and `send_get_peers_response` each build a BitTorrent **compact
/// peer entry** — four IP bytes followed by a big-endian port — from a `nodes` / `peers` array
/// the model supplies. `node.get("port")?.as_u64()? as u16` wraps, so `65536` becomes `0` and
/// `66079` becomes `543`: the querying DHT client is handed a contact it will then dial, on a
/// port nobody named. Bound to `1..=65535` before `to_be_bytes()`, and drop the entry (the
/// surrounding combinator is already a `filter_map` returning `Option`) rather than emitting a
/// nonsense contact.
const NARROWED_MODEL_VALUE_BASELINE: &[&str] = &[
    "server:torrent_dht:actions.rs:231:port",
    "server:torrent_dht:actions.rs:310:port",
];

/// Widths a model-supplied number can silently lose its meaning in.
const NARROWING: &[&str] = &["u8", "u16", "i16", "i32", "u32"];

/// Key-name segments that mean "this number *is* the decision".
///
/// A status, a code, a port, an id, a severity and a count all share the property that a
/// different value is still a *valid* value — which is exactly why a wrap is invisible. A
/// `ttl`, a `sequence` or a `baud_rate` may also be worth bounding, but a wrapped one does not
/// impersonate a decision the model did not make, so they stay out of the vocabulary and out of
/// this build gate.
const KEY_SEGMENTS: &[&str] = &[
    "status", "code", "codes", "port", "ports", "id", "severity", "count",
];

/// Combinators that bound the value while it is still wide.
///
/// `.min(u16::MAX as u64)` saturates rather than wrapping, so the cast that follows is lossless
/// and the peer gets a number the model can be told about. Not as good as a refusal, but it is
/// a deliberate bound rather than arithmetic nobody chose.
const PRE_CAST_GUARDS: &[&str] = &[
    ".min(",
    ".clamp(",
    ".filter(",
    "try_into",
    "try_from",
    "checked_",
    "saturating_",
];

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The operand of the cast at `pos`, as a char range `[start, pos)`.
///
/// Walks backwards balancing brackets and stops where the operand must: an unmatched opener, a
/// statement or argument separator, an `=`, or any binary operator (a cast binds tighter than
/// arithmetic, so `a + b as u16` casts only `b`).
fn chain_before(chars: &[char], pos: usize) -> usize {
    let mut depth = 0i32;
    let mut i = pos;
    while i > 0 {
        let c = chars[i - 1];
        match c {
            ')' | ']' | '}' => depth += 1,
            '(' | '[' | '{' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            ';' | ',' | '=' | '+' | '-' | '*' | '/' | '%' | '|' | '&' | '^' | '<' | '>' | '!'
                if depth == 0 =>
            {
                break
            }
            _ => {}
        }
        i -= 1;
    }
    i
}

/// Every `X.get("key")` / `X["key"]` appearing in a slice of source.
fn json_keys(chunk: &str) -> Vec<String> {
    let b: Vec<char> = chunk.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == '"' {
            // The literal has to be a lookup key: preceded by `.get(` or `[`, followed by `)`
            // or `]`. That rules out every other string in the chain — the `.context("…")`
            // message, an `anyhow!` format, a field name being built.
            let mut j = i + 1;
            while j < b.len() && b[j] != '"' {
                j += 1;
            }
            if j < b.len() {
                let key: String = b[i + 1..j].iter().collect();
                let before: String = b[..i].iter().rev().take(24).collect::<String>();
                let before: String = before.chars().rev().collect();
                let before = before.trim_end();
                let after: String = b[j + 1..].iter().take(8).collect();
                let after = after.trim_start();
                let opens = before.ends_with(".get(") || before.ends_with('[');
                let closes = after.starts_with(')') || after.starts_with(']');
                if opens && closes && !key.is_empty() && key.chars().all(|c| is_ident_char(c)) {
                    out.push(key);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn key_is_a_decision(key: &str) -> bool {
    key.split('_')
        .any(|seg| KEY_SEGMENTS.contains(&seg.to_ascii_lowercase().as_str()))
}

fn has_pre_cast_guard(chunk: &str) -> bool {
    PRE_CAST_GUARDS.iter().any(|g| chunk.contains(g))
}

/// The operand, if it is a bare identifier (optionally `?`, `.unwrap()` or `.clone()`).
fn bare_identifier(chunk: &str) -> Option<String> {
    let t = chunk.trim();
    let t = t
        .strip_suffix('?')
        .or_else(|| t.strip_suffix(".unwrap()"))
        .or_else(|| t.strip_suffix(".clone()"))
        .unwrap_or(t)
        .trim();
    if !t.is_empty() && t.chars().all(is_ident_char) && !t.chars().next().unwrap().is_ascii_digit()
    {
        Some(t.to_string())
    } else {
        None
    }
}

/// Whole-word search for `needle` in `hay`, returning the last match start.
fn rfind_word(hay: &[char], needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.len() > hay.len() {
        return None;
    }
    let mut i = hay.len() - n.len() + 1;
    while i > 0 {
        i -= 1;
        if hay[i..i + n.len()] == n[..] {
            let before_ok = i == 0 || !is_ident_char(hay[i - 1]);
            let after = i + n.len();
            let after_ok = after >= hay.len() || !is_ident_char(hay[after]);
            if before_ok && after_ok {
                return Some(i);
            }
        }
    }
    None
}

/// Char index of the last `let [mut] <name>` in `hay[..before]`.
fn rfind_let_binding(hay: &[char], name: &str, before: usize) -> Option<usize> {
    let mut search = before.min(hay.len());
    while search > 0 {
        let at = rfind_word(&hay[..search], "let")?;
        let rest: String = hay[at + 3..].iter().take(name.len() + 16).collect();
        let decl = rest.trim_start();
        let decl = decl.strip_prefix("mut ").unwrap_or(decl).trim_start();
        if decl.starts_with(name) && !decl[name.len()..].starts_with(is_ident_char) {
            return Some(at);
        }
        search = at;
    }
    None
}

/// Start of the enclosing function: the last `fn ` that begins a line (modulo indentation).
fn enclosing_fn_start(chars: &[char], pos: usize) -> usize {
    let mut i = pos;
    while i >= 3 {
        if chars[i - 3] == 'f' && chars[i - 2] == 'n' && chars[i - 1].is_whitespace() {
            let start = i - 3;
            let mut k = start;
            while k > 0 && (chars[k - 1] == ' ' || chars[k - 1] == '\t') {
                k -= 1;
            }
            if k == 0 || chars[k - 1] == '\n' {
                return start;
            }
        }
        i -= 1;
    }
    0
}

/// A site where a model-supplied decision value is narrowed.
struct Site {
    line: usize,
    key: String,
    guarded: bool,
}

fn scan_file(path: &Path) -> Vec<Site> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let src = strip_comments(&raw);
    let chars: Vec<char> = src.chars().collect();

    // line number for every char index
    let mut line_of = Vec::with_capacity(chars.len() + 1);
    let mut line = 1usize;
    for &c in &chars {
        line_of.push(line);
        if c == '\n' {
            line += 1;
        }
    }
    line_of.push(line);

    let mut sites = Vec::new();
    let mut i = 0usize;
    while i + 2 < chars.len() {
        // a bare `as` token
        if !(chars[i] == 'a'
            && chars[i + 1] == 's'
            && chars[i + 2].is_whitespace()
            && (i == 0 || !is_ident_char(chars[i - 1])))
        {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let mut k = j;
        while k < chars.len() && is_ident_char(chars[k]) {
            k += 1;
        }
        let ty: String = chars[j..k].iter().collect();
        if !NARROWING.contains(&ty.as_str()) {
            i += 1;
            continue;
        }
        let cast_at = i;
        i = k;

        let mut start = chain_before(&chars, cast_at);
        let mut chunk: String = chars[start..cast_at].iter().collect();

        // `.map(|v| v as u16)` — the operand is a closure parameter, so the chain that produced
        // the value is the one the closure is attached to.
        if let Some(name) = bare_identifier(&chunk) {
            let mut p = start;
            while p > 0 && chars[p - 1].is_whitespace() {
                p -= 1;
            }
            if p > 0 && chars[p - 1] == '|' {
                let close = p - 1;
                let mut q = close;
                while q > 0 && chars[q - 1] != '|' {
                    q -= 1;
                }
                let param: String = chars[q..close].iter().collect();
                if q > 0 && param.trim() == name {
                    let mut r = q - 1;
                    while r > 0 && chars[r - 1].is_whitespace() {
                        r -= 1;
                    }
                    if r > 0 && chars[r - 1] == '(' {
                        start = chain_before(&chars, r - 1);
                        chunk = chars[start..r - 1].iter().collect();
                    }
                }
            }
        }

        let keys = json_keys(&chunk);
        if !keys.is_empty() {
            let guarded = has_pre_cast_guard(&chunk);
            for key in keys {
                if key_is_a_decision(&key) {
                    sites.push(Site {
                        line: line_of[cast_at],
                        key,
                        guarded,
                    });
                }
            }
            continue;
        }

        // One `let` binding between the lookup and the cast.
        let Some(name) = bare_identifier(&chunk) else {
            continue;
        };
        let fn_start = enclosing_fn_start(&chars, cast_at);
        let body = &chars[fn_start..cast_at];

        // Walk back through the `let NAME` bindings, newest first. The swept protocols end in
        // `let x = <lookup>; if x > MAX { … } let x = x as u16;`, so the *nearest* `let x` is
        // the shadowing one whose right-hand side is the cast itself — its statement has no
        // terminating `;` inside the body, which is how it is recognised and stepped over.
        let mut before = body.len();
        loop {
            let Some(let_at) = rfind_let_binding(body, &name, before) else {
                break;
            };
            before = let_at;

            let tail: String = body[let_at..].iter().collect();
            let Some(eq) = tail.find('=') else { break };
            let Some(semi) = tail[eq..].find(';') else {
                continue; // the statement the cast is inside; keep walking back
            };
            let rhs = &tail[eq + 1..eq + semi];
            let between = &tail[eq + semi..];

            // Already narrowed at the binding — that cast is a site in its own right and was
            // (or will be) scanned on its own pass, so attributing it here would double-count.
            if NARROWING.iter().any(|t| rhs.contains(&format!("as {t}"))) {
                break;
            }
            let keys = json_keys(rhs);
            if keys.is_empty() {
                break;
            }

            // The exemption, deliberately narrow: the *same binding*, still at its original
            // width, is the thing compared. "The function contains a comparison" exempts
            // everything, which is how an early version of this rule managed to find nothing.
            let compared = ["<", ">", "<=", ">="].iter().any(|op| {
                between.contains(&format!("{name} {op}"))
                    || between.contains(&format!("{name}{op}"))
            }) || between.contains(&format!("contains(&{name})"))
                || between.contains(&format!("matches!({name}"));
            let guarded = compared || has_pre_cast_guard(rhs);

            for key in keys {
                if key_is_a_decision(&key) {
                    sites.push(Site {
                        line: line_of[cast_at],
                        key,
                        guarded,
                    });
                }
            }
            break;
        }
    }
    sites
}

fn actions_files(root: &Path) -> Vec<(String, PathBuf)> {
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
                out.push((
                    parent
                        .strip_prefix(root)
                        .unwrap_or(parent)
                        .to_string_lossy()
                        .to_string(),
                    p.clone(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// `(examined, unguarded)` — every model-supplied decision value narrowed, and the subset with
/// no bound applied first.
fn survey(root: &Path, role: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut examined = BTreeSet::new();
    let mut unguarded = BTreeSet::new();
    for (protocol, path) in actions_files(root) {
        for site in scan_file(&path) {
            let id = format!("{role}:{protocol}:actions.rs:{}:{}", site.line, site.key);
            if !site.guarded {
                unguarded.insert(id.clone());
            }
            examined.insert(id);
        }
    }
    (examined, unguarded)
}

fn survey_both() -> (BTreeSet<String>, BTreeSet<String>) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (mut examined, mut unguarded) = survey(&manifest.join("src/server"), "server");
    let (e2, u2) = survey(&manifest.join("src/client"), "client");
    examined.extend(e2);
    unguarded.extend(u2);
    (examined, unguarded)
}

// ---------------------------------------------------------------------------
// The ratchet
// ---------------------------------------------------------------------------

#[test]
fn no_model_supplied_code_is_narrowed_without_a_range_check_first() {
    let (_, unguarded) = survey_both();

    let found: BTreeSet<&str> = unguarded.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = NARROWED_MODEL_VALUE_BASELINE.iter().copied().collect();

    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these narrow a value the model supplied with no bound applied first, so an \
         out-of-range value silently becomes an in-range one: {new:?}\n\
         `status_code: 65736` is `200` after `as u16`; LDAP's `result_code: 256` is `0`, which \
         is `success`. Read the value at its full width, refuse anything outside the range the \
         protocol defines (naming the range and the codes that matter, because that message is \
         what the model's repair loop reads), and cast only then. A check placed *after* the \
         cast does not count and is not an oversight this test can see — `65836 as u16` is 300, \
         which passes a `300..700` check."
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these are no longer unguarded — remove them from NARROWED_MODEL_VALUE_BASELINE so the \
         ratchet keeps its grip: {fixed:?}"
    );
}

/// A scan that silently matches nothing passes forever. This asserts it is reading real code.
#[test]
fn the_scan_finds_narrowing_casts_to_check() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (root, min_files) in [("src/server", 100usize), ("src/client", 80)] {
        let files = actions_files(&manifest.join(root));
        assert!(
            files.len() >= min_files,
            "{root}: found {} actions.rs files, expected at least {min_files}",
            files.len()
        );
    }

    let (examined, unguarded) = survey_both();
    assert!(
        examined.len() >= 20,
        "only {} model-supplied status/code/port/id/severity/count values are narrowed anywhere \
         in the tree. The whole tree had ~30 when this was written, so a number this low means \
         the anchor stopped resolving `action.get(\"…\")` chains rather than that the code got \
         better: {examined:?}",
        examined.len()
    );

    // The exemption has to be doing real work, and it has to be *narrow*. If it ever exempted
    // everything — the failure mode an early attempt had, where "the enclosing function contains
    // a comparison" matched almost every function body — the ratchet above would go green while
    // checking nothing, and nothing else would notice.
    let exempted = examined.len() - unguarded.len();
    assert!(
        exempted >= 15,
        "only {exempted} of {} sites are recognised as bounded before the cast. The swept \
         protocols each end in `if raw > MAX {{ return Err(..) }}; raw as u16`, so this should \
         count them all — a drop means the exemption stopped seeing the fix that was applied.",
        examined.len()
    );
}

/// What the rule does, on inputs whose right answer is known, rather than on the tree.
///
/// The tree is the ratchet's subject; it cannot also be its test. These eight snippets are the
/// shapes the sweep actually met, written out so that a change to `scan_file` that inverts the
/// rule fails here loudly instead of turning the ratchet above into a no-op.
#[test]
fn the_rule_flags_the_defect_and_not_its_fix() {
    fn sites(body: &str) -> Vec<(String, bool)> {
        let dir = std::env::temp_dir().join(format!(
            "netget-narrowing-cast-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("actions.rs");
        std::fs::write(&f, body).unwrap();
        let out = scan_file(&f)
            .into_iter()
            .map(|s| (s.key, s.guarded))
            .collect();
        let _ = std::fs::remove_file(&f);
        out
    }

    // 1. The bare defect: straight from the model's JSON to a narrower type.
    assert_eq!(
        sites("fn f() { let s = action.get(\"status_code\").and_then(|v| v.as_u64()).unwrap_or(200) as u16; }"),
        vec![("status_code".to_string(), false)],
        "the plain `action.get(k)… as u16` chain must be flagged"
    );

    // 2. The fix: bound at full width, then cast.
    assert_eq!(
        sites(
            "fn f() {\n\
             let raw = action.get(\"status_code\").and_then(|v| v.as_u64()).unwrap_or(200);\n\
             if raw > 599 { return Err(e); }\n\
             let s = raw as u16;\n}"
        ),
        vec![("status_code".to_string(), true)],
        "a comparison on the binding, at its original width, is the fix and must be exempt"
    );

    // 3. The `stun` trap: the same check, placed after the cast. `65836 as u16` is 300.
    assert_eq!(
        sites(
            "fn f() {\n\
             let s = action.get(\"error_code\").and_then(|v| v.as_u64()).unwrap_or(400) as u16;\n\
             if !(300..700).contains(&s) { return Err(e); }\n}"
        ),
        vec![("error_code".to_string(), false)],
        "a range check *after* the narrowing is not a fix and must still be flagged"
    );

    // 4. Saturation in the chain: deliberate, lossless, and not this class.
    assert_eq!(
        sites("fn f() { let c = action.get(\"message_count\").and_then(|v| v.as_u64()).unwrap_or(0).min(u32::MAX as u64) as u32; }"),
        vec![("message_count".to_string(), true)],
        "`.min(..)` bounds the value while it is still wide"
    );

    // 5. The closure form, which hides the chain behind a parameter name.
    assert_eq!(
        sites("fn f() { let id = action.get(\"server_id\").and_then(|v| v.as_u64()).map(|v| v as u32); }"),
        vec![("server_id".to_string(), false)],
        "`.map(|v| v as u32)` must resolve back to the chain the closure is attached to"
    );

    // 6. Length arithmetic — the reason the anchor is the lookup and not the cast. This is
    //    what a rule keyed on `as u16` alone would drown in.
    assert!(
        sites("fn f() { buf.extend_from_slice(&(payload.len() as u16).to_be_bytes()); }")
            .is_empty(),
        "a length narrowed for a wire field has no model-supplied value in it at all"
    );

    // 7. A key outside the vocabulary. A wrapped TTL is wrong; it does not impersonate a
    //    decision the model did not make, so it is not this gate's business.
    assert!(
        sites("fn f() { let t = action.get(\"ttl\").and_then(|v| v.as_u64()).unwrap_or(300) as u32; }").is_empty(),
        "the vocabulary is status/code/port/id/severity/count and nothing else"
    );

    // 8. The fix as several protocols actually wrote it: the checked binding is *shadowed* by
    //    the narrowed one. The nearest `let status_code` is then the cast's own statement, and
    //    a scan that stops there sees no lookup and silently drops the site — which would leave
    //    the fixed protocols unwatched, so removing their check again would not fail anything.
    assert_eq!(
        sites(
            "fn f() {\n\
             let status_code = action.get(\"status_code\").and_then(|v| v.as_u64()).unwrap_or(200);\n\
             if !(100..=599).contains(&status_code) { return Err(e); }\n\
             let status_code = status_code as u16;\n}"
        ),
        vec![("status_code".to_string(), true)],
        "a shadowed rebinding must resolve back to the binding that read the JSON"
    );

    // 9. A string in the chain that merely looks like a lookup. The `.context(..)` message of
    //    every swept site mentions the field name; reading it as a second lookup would double
    //    every finding.
    assert_eq!(
        sites("fn f() { let s = action.get(\"status\").and_then(|v| v.as_u64()).context(\"Missing 'code' or 'port'\")? as u16; }"),
        vec![("status".to_string(), false)],
        "only a literal in `.get(\"k\")` or `[\"k\"]` position counts as a lookup"
    );
}
