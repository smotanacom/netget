//! A budget must be decided by the handler's **answer**, never by its configuration.
//!
//! `tuntap` skipped its LLM budget whenever a script handler was *configured*. The reasoning was
//! sound as far as it went — a script answers in-process and costs no model call, so charging it
//! to a per-minute budget would be wrong. What it missed is that
//! `execute_script_handler` returns `FallbackToLlm` when the language is not installed, when the
//! language name is unknown, or when the script throws. By then the budget gate had already been
//! skipped on the strength of the handler merely existing.
//!
//! The shipped script-mode startup example, on a box without `python3`, was therefore **one
//! uncounted model call per admitted packet, at wire rate**, with `llm_escalation: "never"`
//! inert and the counters cheerfully reporting `handled_by_rule`. Nothing errors. The only
//! visible symptom is the LLM bill.
//!
//! The fix is not a better guess about which handler types are safe. It is to **run the
//! handler and branch on what comes back**: `Handled` costs nothing and is used as it stands;
//! `FallbackToLlm` falls through to the budget like any unclaimed packet. `tuntap` now does
//! that, and its comment at the site explains why.
//!
//! # The rule, and why it is deliberately narrow
//!
//! `PROTOCOL_QUALITY.md` says of this class: *"Hard to scan generically; scan for the idiom."*
//! That is right, and the reason is that "a budget" has no syntactic signature — the thing being
//! protected could be a counter, a semaphore, a rate limiter or a boolean. What *does* have a
//! signature is the mistake: a file that owns a bound, and decides whether to apply it by
//! looking at configuration rather than at a result.
//!
//! So a file under `src/server` or `src/client` is flagged when all three hold:
//!
//! 1. it owns a **bound** — one of [`BOUND_MARKERS`] (`try_take(`, `max_per_minute`,
//!    `LlmEscalation`, …). A file with no budget has nothing to skip;
//! 2. it **inspects handler configuration** — one of [`CONFIGURATION_PEEKS`] (`find_handler(`,
//!    `has_script_for_event(`, `get_event_handler_config(`, …);
//! 3. it does **not** dispatch on the result — none of [`RESULT_BASED_DISPATCH`]
//!    (`EventHandlerResult`, `FallbackToLlm`, `try_execute_event_handler`).
//!
//! Two files in the whole tree own a per-instance LLM budget, so this rule looks at two files.
//! That is not a weakness: a narrow rule with no false positives is worth more than a broad one
//! people learn to ignore, and `CLAUDE.md` is explicit that a noisy build-failing check trains
//! people to edit the baseline instead of the code. What it costs is coverage of budgets built
//! out of vocabulary this list does not know — so **if you add a rate-limited protocol, add its
//! marker here**, and the rule will start watching it.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test handler_result_decides_the_bound_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `role:protocol:file` for every budget gated on configuration rather than on a result.
///
/// **Empty, and it must stay empty.** `server:rtp:mod.rs` was its only entry and was fixed on
/// 15 September 2026 rather than re-baselined: `RtpServer::handle_datagram` now calls
/// `try_execute_event_handler` and branches on `Handled` versus `FallbackToLlm`, so a `Script`
/// rule that cannot answer — no `python3`, an unknown language name, a script that threw —
/// falls through to the rolling `RtpLlmBudget` like any unclaimed datagram instead of buying an
/// exemption from it.
///
/// It was measured rather than argued, by reinstating the old gate and counting the calls a
/// recording mock model received: five datagrams at `llm_max_per_minute: 0` produced **five**
/// consultations before, and **zero** after. `tests/server/rtp/script_fallback_budget_test.rs`
/// keeps that measurement, including the ample-ceiling control that makes the zero mean
/// something.
///
/// `Static` and `Manual` were always safe to exempt — a static handler with an empty `actions`
/// array provably suppresses the call (`tests/empty_static_handler_test.rs`), and a manual one
/// parks or fails closed — but exempting them by *type* is the same reasoning that made
/// `Script` leak, so the repaired gate does not distinguish: whatever answers is free, whatever
/// declines is charged.
const CONFIGURATION_GATED_BUDGET_BASELINE: &[&str] = &[];

/// Tokens that mean "this file owns a bound it could skip".
///
/// Keep these specific to a *per-instance LLM budget*. A generic word like `limit` would match
/// half the tree and the rule would become a baseline nobody reads.
const BOUND_MARKERS: &[&str] = &[
    "try_take(",
    "max_per_minute",
    "llm_max_per_minute",
    "LlmEscalation",
    "llm_escalation",
    "dropped_over_budget",
    "escalated_to_llm",
];

/// Tokens that mean "this file is asking what is *configured*".
const CONFIGURATION_PEEKS: &[&str] = &[
    "find_handler(",
    "has_script_for_event(",
    "get_event_handler_config(",
    "event_handler_config",
];

/// Tokens that mean "this file is asking what the handler *answered*".
const RESULT_BASED_DISPATCH: &[&str] = &[
    "EventHandlerResult",
    "FallbackToLlm",
    "try_execute_event_handler",
    "try_execute_client_event_handler",
];

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

/// Remove `//` comments without cutting inside a string literal.
///
/// Comments matter here more than in most scans, because the file this rule is *about* explains
/// the correct design at length in prose. `tuntap`'s header names `FallbackToLlm` in a comment
/// eight lines before the code does; counting that would have exempted `rtp` too, whose own
/// comment likewise names the shape it no longer implements.
fn strip_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < b.len() {
        let c = b[i];
        if in_string {
            if c == '\\' && i + 1 < b.len() {
                // Blank both halves, but keep a newline: Rust's line continuation (a `\`
                // at end of line inside a literal) is an escape whose second character IS
                // the newline, and swallowing it silently shifted every line number below
                // it — nine of them in `ipp/actions.rs` alone.
                out.push(' ');
                out.push(if b[i + 1] == '\n' { '\n' } else { ' ' });
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            out.push(c);
            i += 1;
            continue;
        }
        // A char literal may *be* a quote (`'\"'`), and reading it as the start of a
        // string inverts the quote parity for the rest of the file. Lifetimes (`'a`) are
        // left alone, which is why the closing `'` has to be where a char literal puts it.
        if c == '\'' {
            let simple = i + 2 < b.len() && b[i + 2] == '\'';
            let escaped = i + 3 < b.len() && b[i + 1] == '\\' && b[i + 3] == '\'';
            if simple || escaped {
                let n = if simple { 3 } else { 4 };
                for k in 0..n {
                    out.push(if b[i + k] == '\n' { '\n' } else { ' ' });
                }
                i += n;
                continue;
            }
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `(owns a bound, peeks at configuration, dispatches on a result)`.
fn classify(src: &str) -> (bool, bool, bool) {
    let code = strip_comments(src);
    (
        BOUND_MARKERS.iter().any(|m| code.contains(m)),
        CONFIGURATION_PEEKS.iter().any(|m| code.contains(m)),
        RESULT_BASED_DISPATCH.iter().any(|m| code.contains(m)),
    )
}

fn is_configuration_gated(src: &str) -> bool {
    let (bound, peek, result) = classify(src);
    bound && peek && !result
}

fn rust_files(root: &Path) -> Vec<(String, String, PathBuf)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let parent = p.parent().unwrap();
                out.push((
                    parent
                        .strip_prefix(root)
                        .unwrap_or(parent)
                        .to_string_lossy()
                        .to_string(),
                    p.file_name().unwrap().to_string_lossy().to_string(),
                    p,
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// `(files owning a bound, the configuration-gated subset)`.
fn survey() -> (BTreeSet<String>, BTreeSet<String>) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut with_bound = BTreeSet::new();
    let mut gated = BTreeSet::new();
    for (role, dir) in [("server", "src/server"), ("client", "src/client")] {
        for (protocol, file, path) in rust_files(&manifest.join(dir)) {
            if protocol.is_empty() {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (bound, peek, result) = classify(&src);
            let id = format!("{role}:{protocol}:{file}");
            if bound && peek {
                with_bound.insert(id.clone());
                if !result {
                    gated.insert(id);
                }
            }
        }
    }
    (with_bound, gated)
}

// ---------------------------------------------------------------------------
// The ratchet
// ---------------------------------------------------------------------------

#[test]
fn no_budget_is_gated_on_handler_configuration() {
    let (_, gated) = survey();
    let found: BTreeSet<&str> = gated.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = CONFIGURATION_GATED_BUDGET_BASELINE
        .iter()
        .copied()
        .collect();

    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these decide whether to apply an LLM budget by looking at what is *configured* rather \
         than at what the handler *answered*: {new:?}\n\
         A Script handler is exempt from a budget because it answers in-process — but \
         `execute_script_handler` returns `FallbackToLlm` when the language is not installed, \
         when the language name is unknown, or when the script throws, and by then the gate has \
         already been skipped. `tuntap`'s shipped script-mode example on a box without \
         `python3` was one uncounted model call per packet at wire rate, with \
         `llm_escalation: \"never\"` inert. Call `try_execute_event_handler` and branch on \
         `EventHandlerResult::Handled` versus `FallbackToLlm`: the answer decides, not the \
         configuration."
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these now dispatch on the handler's result — remove them from \
         CONFIGURATION_GATED_BUDGET_BASELINE: {fixed:?}"
    );
}

/// The rule's whole force comes from the two budget-owning protocols being *seen and passed*.
///
/// The gate is narrow — two files in the tree own a per-instance LLM budget — so if
/// [`BOUND_MARKERS`] ever stopped matching, `survey` would return an empty set and the test
/// above would go green while checking nothing at all. This asserts both subjects still exist
/// and that both are recognised as correct.
///
/// `rtp` is checked by reading its source directly rather than through `survey`, because the
/// repair left it with no configuration peek *in the budget path* at all — only the
/// diagnostics-only `configured_handler_kind`, which is what keeps it in the examined set. A
/// bare `with_bound.len() >= 2` would therefore be satisfied by the wrong thing the day that
/// helper is inlined; `classify` says the two things that actually matter.
#[test]
fn the_scan_sees_the_protocols_that_get_this_right() {
    let (with_bound, gated) = survey();
    for known_good in ["server:tuntap:mod.rs", "server:rtp:mod.rs"] {
        assert!(
            with_bound.contains(known_good),
            "{known_good} owns a per-minute LLM budget and consults handler configuration, so \
             it must be in the examined set; the scan currently sees {with_bound:?}"
        );
        assert!(
            !gated.contains(known_good),
            "{known_good} dispatches on EventHandlerResult and must be recognised as correct — \
             if this fails, the repair has been undone"
        );
    }

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let rtp = std::fs::read_to_string(manifest.join("src/server/rtp/mod.rs"))
        .expect("src/server/rtp/mod.rs must exist");
    let (bound, _peek, result) = classify(&rtp);
    assert!(bound, "rtp still owns the rolling RtpLlmBudget");
    assert!(
        result,
        "rtp must decide that budget from the handler's answer, not from its configuration"
    );
}

/// What the rule does on inputs whose right answer is known.
#[test]
fn the_rule_flags_the_historical_defect_and_not_its_fix() {
    // 1. `tuntap` as it was: gate 3 skipped because a handler is configured.
    let before = "
        struct Budget;
        impl Budget { fn try_take(&mut self) -> bool { true } }
        async fn handle(&self) {
            let handled = self.state.get_event_handler_config(id).await
                .map(|c| c.find_handler(EVENT.id.as_str()).is_some())
                .unwrap_or(false);
            if !handled {
                if !self.budget.try_take() { return; }
            }
            call_llm(&event).await;
        }";
    assert!(
        is_configuration_gated(before),
        "skipping a budget because a handler exists is the defect and must be flagged"
    );

    // 2. `tuntap` as it is: run the handler, branch on the answer.
    let after = "
        struct Budget;
        impl Budget { fn try_take(&mut self) -> bool { true } }
        async fn handle(&self) {
            let outcome = try_execute_event_handler(&self.state, id, None, EVENT.id.as_str()).await;
            match outcome {
                Ok(EventHandlerResult::Handled(r)) => r,
                Ok(EventHandlerResult::FallbackToLlm { .. }) => {
                    if !self.budget.try_take() { return; }
                    call_llm(&event).await
                }
                Err(_) => return,
            };
        }";
    assert!(
        !is_configuration_gated(after),
        "branching on EventHandlerResult is the fix and must not be reported"
    );

    // 3. No bound at all — most protocols. Consulting handler configuration is perfectly normal
    //    (the 16 `operator_wants_dynamic` servers do it); it is only a defect when there is a
    //    bound being skipped on the strength of it.
    assert!(
        !is_configuration_gated(
            "async fn operator_wants_dynamic(state: &AppState) -> bool {
                 server.event_handler_config.as_ref()
                     .map(|c| c.find_handler(event_id).is_some()).unwrap_or(false)
             }"
        ),
        "inspecting configuration without a bound to skip is not this class"
    );

    // 4. A bound with no configuration peek — nothing can bypass it.
    assert!(
        !is_configuration_gated("fn handle() { if !budget.try_take() { return; } call_llm(); }"),
        "a bound that is always applied is the point, not the defect"
    );

    // 5. Comments must not exempt. `rtp`'s own doc comment claims the shape it no longer has
    //    ('Same shape as tuntap::a_rule_answers'), and `tuntap`'s header names FallbackToLlm in
    //    prose eight lines before the code does. Counting either would have exempted rtp.
    let commented =
        format!("{before}\n// this should really use EventHandlerResult::FallbackToLlm");
    assert!(
        is_configuration_gated(&commented),
        "a comment naming the correct design does not implement it"
    );
    // ...but a string literal is code, and must survive the stripper.
    assert!(
        strip_comments("let s = \"http://x\"; // c").contains("http://x"),
        "the stripper must not cut at the // inside a literal"
    );
}
