//! A test count stated in a `tests/**/CLAUDE.md` must be the number of tests that are there.
//!
//! "22 tests" was wrong in three files when Programme 2 read them by hand, and a number that
//! has to be re-typed whenever anyone adds a test will be wrong again. The obvious fixes are
//! both bad: a script that regenerates the numbers is one more thing nobody runs, and banning
//! counts outright deletes the useful ones — `tests/server/can/CLAUDE.md`'s opening line ("43
//! tests, all passing, no `#[ignore]`s. 28 in `frame_test.rs`, 15 in `e2e_test.rs`") tells a
//! reader in one sentence what the suite is.
//!
//! So this test takes the third option: **a count is allowed exactly where its scope is
//! unambiguous, and there it is checked.** Three shapes qualify.
//!
//! 1. **A file-scoped count** — `` `e2e_test.rs` — 6 tests ``, `` `action_test.rs` (14 tests,
//!    …) ``, `` (6 tests in `test.rs`) `` — is checked against that file. Headings count: `` ##
//!    `decode_test.rs` — 14 tests `` is where most of these live.
//! 2. **A count that begins a line** — `51 tests, all passing, none `#[ignore]`d.` — is checked
//!    against the enclosing scope: the single `.rs` file the nearest heading above it names, or
//!    the whole directory when the heading names none.
//! 3. **A bare breakdown on such a line** — `28 in `frame_test.rs`, 15 in `e2e_test.rs`` — is
//!    checked per file. The bare number is only read as a test count when the line opened with
//!    a count of its own, which is what makes it unambiguous.
//!
//! Everything else is left alone, deliberately. 107 occurrences of "N tests" exist across these
//! docs and most sit inside a runtime estimate or an LLM-call budget ("~40-50 seconds for full
//! test suite (4 tests × ~10s each)", "6 LLM calls across 3 tests"), where the count is
//! incidental to a figure that is itself an estimate nothing can verify. Three more are verbatim
//! `running 5 tests` cargo transcripts. Failing the build on those would be ceremony: it would
//! force an edit to prose whose point is not the number.
//!
//! # False-positive rate
//!
//! Zero, measured. 32 claims qualify under the three rules, and 5 disagreed with the source.
//! All five were stale docs, not miscounts by this test: `datalink/action_test.rs` (15 claimed,
//! 14 there), `ssh_agent/test.rs` (5/6) and its `e2e_test.rs` (4/5), `stp` (27/28) and `tuntap`
//! (48/51). They are fixed in the commit before this one. The count of claims is printed on
//! every run, so a rewording that drops a claim out of scope instead of fixing it is visible.
//!
//! Three narrowings buy that zero, and each was added after a measured false positive:
//!
//! - **Scope comes from the nearest heading.** Restricting line-initial counts to the doc's
//!   first section was tried first and cost four correct checks; the heading rule keeps them
//!   and still excludes `tests/server/db2/CLAUDE.md`'s "8 tests, no LLM calls, sub-millisecond",
//!   which sits under a `` ## `drda_test.rs` `` heading and means that file's eight, not the
//!   directory's ten.
//! - **List items are excluded.** `- **4 tests × 3 seconds**: ~12 seconds` is a runtime
//!   estimate; three of those disagree with their directory total while meaning something
//!   narrower.
//! - **The noun must be plural unless the count is 1.** `tests/server/bluetooth_ble/CLAUDE.md`
//!   says "**3 test cases**" about one file under a heading that names none, and reading "test"
//!   as the noun for any count turned that into a claim about the whole directory.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test doc_test_counts_test

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Source with `//` line comments and `/* … */` block comments removed.
///
/// Block comments matter: `mqtt`'s four pub/sub tests sat inside a `/* … */` for months, so
/// nothing compiled them and `--include-ignored` could never have run them. A counter that
/// counted them would report tests that do not exist — the exact failure this test is for.
fn strip_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `#[test]` and `#[tokio::test…]` attributes at the start of a line.
///
/// `#[ignore]`d tests count: the claim is about how many tests the file holds, not how many run.
fn tests_in_file(path: &Path) -> usize {
    let src = strip_comments(&std::fs::read_to_string(path).unwrap_or_default());
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            let Some(rest) = t.strip_prefix("#[") else {
                return false;
            };
            let rest = rest.strip_prefix("tokio::").unwrap_or(rest);
            let Some(after) = rest.strip_prefix("test") else {
                return false;
            };
            // `test]` or `test(flavor = …)]`, but not `test_case` or `test_util`.
            matches!(after.chars().next(), Some(']') | Some('('))
        })
        .count()
}

fn tests_in_dir(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "rs"))
        .collect();
    paths.sort();
    paths.iter().map(|p| tests_in_file(p)).sum()
}

/// Every `CLAUDE.md` under `tests/`.
fn test_docs(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name().is_some_and(|n| n == "CLAUDE.md") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("tests"), &mut out);
    out.sort();
    out
}

/// The backticked spans of one line, with their end offsets.
fn backticked(line: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let mut base = 0usize;
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after_start = base + open + 1;
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) => {
                out.push((&after[..close], after_start + close + 1));
                base = after_start + close + 1;
                rest = &line[base..];
            }
            None => break,
        }
    }
    out
}

/// The first `<digits> test(s)` in `text`, as the number.
///
/// Scanning left to right and requiring the digits to be followed directly by whitespace and
/// `test` is what makes a table cell like `| **14** across 3 tests |` yield 3 rather than 14.
fn first_test_count(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let after = &text[i..];
        let trimmed = after.trim_start();
        if after.len() != trimmed.len() {
            if let Some(n) = count_if_followed_by_test_noun(&text[start..i], trimmed) {
                return Some(n);
            }
        }
    }
    None
}

/// `digits` parsed, but only when `rest` begins with the bare noun this test can check.
///
/// The noun must be "tests", or "test" when the count is 1. That plural rule is load-bearing:
/// `tests/server/bluetooth_ble/CLAUDE.md` says "**3 test cases**" about one file, under a
/// heading that names none, and accepting "test" for any count made that read as a claim about
/// the whole directory — the only false positive this check has produced.
fn count_if_followed_by_test_noun(digits: &str, rest: &str) -> Option<usize> {
    let n: usize = digits.parse().ok()?;
    let tail = match rest.strip_prefix("tests") {
        Some(tail) => tail,
        None => {
            if n != 1 {
                return None;
            }
            rest.strip_prefix("test")?
        }
    };
    if tail.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(n)
}

/// The `N` of a text ending in `N tests in ` (or `N test in `, for N = 1).
///
/// The whole phrase has to be at the end, so the number is the one the sentence attaches to the
/// filename that follows.
fn trailing_test_count_before_in(before: &str) -> Option<usize> {
    let head = before.trim_end();
    let head = head.strip_suffix("in")?;
    if head
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let head = head.trim_end();
    let (head, noun) = match head.strip_suffix("tests") {
        Some(h) => (h, "tests"),
        None => (head.strip_suffix("test")?, "test"),
    };
    let head = head.trim_end();
    let digits_start = head.trim_end_matches(|c: char| c.is_ascii_digit());
    if digits_start.len() == head.len() {
        return None;
    }
    count_if_followed_by_test_noun(&head[digits_start.len()..], noun)
}

/// The `N` of a text ending in `N in `, with no noun at all.
fn bare_count_before_in(before: &str) -> Option<usize> {
    let head = before.trim_end().strip_suffix("in")?;
    if head
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let head = head.trim_end();
    let digits_start = head.trim_end_matches(|c: char| c.is_ascii_digit());
    if digits_start.len() == head.len() {
        return None;
    }
    head[digits_start.len()..].parse().ok()
}

/// A claim a doc makes about how many tests something holds.
struct Claim {
    doc: String,
    line: usize,
    scope: String,
    claimed: usize,
    actual: usize,
}

/// The `.rs` file a heading names, if it names exactly one that exists next to the doc.
fn heading_scope(heading: &str, doc_dir: &Path) -> Option<PathBuf> {
    let named: Vec<PathBuf> = backticked(heading)
        .into_iter()
        .filter(|(s, _)| s.ends_with(".rs"))
        .filter_map(|(s, _)| Path::new(s).file_name().map(|n| doc_dir.join(n)))
        .filter(|p| p.is_file())
        .collect();
    match named.len() {
        1 => Some(named[0].clone()),
        _ => None,
    }
}

/// How many characters may sit between a filename and the count that belongs to it.
///
/// 25 covers every spelling in the tree, the widest being the `oci_registry` table row
/// `| `e2e_test.rs` | **14** across 3 tests |`. Wider would start pairing a filename with a
/// number from the next clause.
const FILE_COUNT_WINDOW: usize = 25;

#[test]
fn every_scoped_test_count_in_a_doc_matches_the_source() {
    let root = repo_root();
    let docs = test_docs(&root);
    assert!(
        docs.len() > 100,
        "only {} docs under tests/ — the walk is not seeing the tree",
        docs.len()
    );

    let mut claims: Vec<Claim> = Vec::new();

    for doc in &docs {
        let doc_rel = doc
            .strip_prefix(&root)
            .unwrap_or(doc)
            .to_string_lossy()
            .replace('\\', "/");
        let doc_dir = doc.parent().unwrap_or(Path::new("."));
        let text = std::fs::read_to_string(doc).unwrap_or_default();
        let mut scope: Option<PathBuf> = None;

        for (idx, line) in text.lines().enumerate() {
            let lineno = idx + 1;

            if line.starts_with('#') {
                scope = heading_scope(line, doc_dir);
                // A heading is not exempt: `## `decode_test.rs` — 14 tests` is where most
                // file-scoped counts in this tree actually live, so rule (1) still runs on it.
            }

            // (1) File-scoped: a backticked `*.rs` with a count just after it, or
            //     `N tests in `file.rs``.
            for (span, end) in backticked(line) {
                if !span.ends_with(".rs") {
                    continue;
                }
                let Some(name) = Path::new(span).file_name() else {
                    continue;
                };
                let file = doc_dir.join(name);
                if !file.is_file() {
                    continue;
                }

                let after = &line[end.min(line.len())..];
                let mut cap = after.len().min(FILE_COUNT_WINDOW);
                while cap < after.len() && !after.is_char_boundary(cap) {
                    cap += 1;
                }
                let window = &after[..cap];
                if let Some(n) = first_test_count(window) {
                    claims.push(Claim {
                        doc: doc_rel.clone(),
                        line: lineno,
                        scope: format!("`{}`", name.to_string_lossy()),
                        claimed: n,
                        actual: tests_in_file(&file),
                    });
                    continue;
                }

                // `N tests in `file.rs`` — the count sits immediately before the name. It must
                // be immediate: `can`'s opening line is "**43 tests, … .** 28 in
                // `frame_test.rs`, 15 in `e2e_test.rs`", where a loose scan pairs `frame_test.rs`
                // with the 43 that belongs to the directory.
                let before = &line[..end.saturating_sub(span.len() + 2)];
                // A bare `N in `file.rs`` counts only when the line opened with a count of its
                // own — "43 tests, all passing. 28 in `frame_test.rs`, 15 in `e2e_test.rs`" —
                // which is what makes the bare number a test count rather than anything else.
                let opens_with_count =
                    first_test_count_at_start(line.trim_start_matches('*')).is_some();
                if let Some(n) = trailing_test_count_before_in(before).or_else(|| {
                    opens_with_count
                        .then(|| bare_count_before_in(before))
                        .flatten()
                }) {
                    claims.push(Claim {
                        doc: doc_rel.clone(),
                        line: lineno,
                        scope: format!("`{}`", name.to_string_lossy()),
                        claimed: n,
                        actual: tests_in_file(&file),
                    });
                }
            }

            // (2) A count that begins the line is a claim about the enclosing scope. List items
            //     are excluded: those are runtime estimates whose count means something
            //     narrower than the line they sit on.
            let head = line.trim_start_matches('*');
            if head.len() == line.len() || line.starts_with("**") {
                if let Some(n) = first_test_count_at_start(head) {
                    let (scope_name, actual) = match &scope {
                        Some(f) => (
                            format!("`{}`", f.file_name().unwrap_or_default().to_string_lossy()),
                            tests_in_file(f),
                        ),
                        None => ("the directory".to_string(), tests_in_dir(doc_dir)),
                    };
                    claims.push(Claim {
                        doc: doc_rel.clone(),
                        line: lineno,
                        scope: scope_name,
                        claimed: n,
                        actual,
                    });
                }
            }
        }
    }

    // Visible with --nocapture: how much of the tree this actually covers. A count that drops
    // sharply means a rewording slipped a claim out of scope rather than fixing it.
    println!(
        "checked {} scoped test-count claim(s) across {} docs under tests/",
        claims.len(),
        docs.len()
    );
    assert!(
        claims.len() >= 20,
        "only {} scoped test-count claims found — the extractor has stopped working",
        claims.len()
    );

    let wrong: Vec<String> = claims
        .iter()
        .filter(|c| c.claimed != c.actual)
        .map(|c| {
            format!(
                "{}:{}: claims {} tests in {}, source has {}",
                c.doc, c.line, c.claimed, c.scope, c.actual
            )
        })
        .collect();

    assert!(
        wrong.is_empty(),
        "{} test-count claim(s) disagree with the source:\n\n{}\n\n\
         Correct the number, or reword so the count has no scope this test can pin — a count \
         inside a runtime estimate or an LLM-call budget is not checked.",
        wrong.len(),
        wrong.join("\n"),
    );
}

/// `first_test_count`, but the digits must be the very start of the text.
fn first_test_count_at_start(text: &str) -> Option<usize> {
    if !text.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let rest = text[end..].trim_start();
    if rest.len() == text.len() - end {
        return None; // no whitespace between the number and what follows
    }
    count_if_followed_by_test_noun(&text[..end], rest)
}
