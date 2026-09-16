//! Real-model eval: can a model actually drive these protocols?
//!
//! Skips unless `NETGET_USE_OLLAMA=1`. See `tests/eval/mod.rs` for the design and
//! `./run-eval.sh` for the way this is meant to be invoked.
//!
//! **This is not a gate.** It reports a score and writes it to
//! `eval-results/latest.json` and `EVAL_RESULTS.md`; it fails only when the
//! *harness* could not run — no Ollama, no model, no cases compiled in — because
//! a nightly that silently measures nothing is the failure mode this repository
//! keeps finding in its own suite.

pub mod helpers;

#[path = "eval/mod.rs"]
mod eval;

use helpers::common::E2EResult;
use helpers::llm_live::{ensure_model_available, live_llm_enabled, live_model};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_model_eval() -> E2EResult<()> {
    if !live_llm_enabled() {
        return Ok(());
    }

    let model = live_model();

    // Wait for Ollama before `ensure_model_available` asks it anything.
    //
    // A sweep is hours long, and its first act must not be a single request
    // against a daemon that may be mid-model-swap: `/api/tags` can be
    // unresponsive for minutes, and a whole run has already been lost to that
    // one call failing at step 1 against an Ollama that came back moments later.
    // The helper's own bound is now generous (`OLLAMA_PROBE_TIMEOUT`, one shared
    // client), but a bound is still one attempt and this is a poll.
    if !eval::runner::wait_for_ollama_ready(std::time::Duration::from_secs(300)).await {
        return Err(
            "Ollama did not answer /api/tags within 300s — it is down or wedged, \
                    so this run would measure nothing"
                .into(),
        );
    }
    ensure_model_available(&model).await?;

    let cases = eval::suites::all_cases();
    if cases.is_empty() {
        return Err(
            "no eval cases compiled in — build with the protocol features you \
                    want to evaluate, e.g. --features http,dns,tcp"
                .into(),
        );
    }

    let runs = eval::runner::runs_per_case();
    let results = eval::runner::run_suite(&cases).await?;

    if results.is_empty() {
        return Err(format!(
            "NETGET_EVAL_PROTOCOLS={:?} selected no case out of {} compiled in",
            eval::runner::protocol_filter(),
            cases.len()
        )
        .into());
    }

    let report = eval::report::build(&model, runs, results);
    eval::report::print_summary(&report);

    // Every attempted case errored at the harness level → the run measured
    // nothing and must say so loudly rather than publishing a 0% score as
    // though the model had been asked.
    let attempted: Vec<_> = report
        .cases
        .iter()
        .filter(|c| c.status == "attempted")
        .collect();
    let all_harness_errors = !attempted.is_empty()
        && attempted
            .iter()
            .all(|c| c.runs.iter().all(|r| r.verdict == "error"));

    let written = eval::report::write(&report)?;
    for path in &written {
        println!("📄 wrote {}", path.display());
    }

    if all_harness_errors {
        return Err(
            "every attempted case failed at the harness level (server start or \
                    client spawn) — the model was never asked, so this run measured \
                    nothing"
                .into(),
        );
    }

    // Opt-in gate for anyone who wants one. Off by default, on purpose: a
    // failing eval is a finding about an action description, not a broken build.
    if let Ok(min) = std::env::var("NETGET_EVAL_MIN_RATE") {
        let min: f64 = min.parse().map_err(|_| {
            format!(
                "NETGET_EVAL_MIN_RATE must be a fraction like 0.7, got {:?}",
                min
            )
        })?;
        if report.totals.pass_rate < min {
            return Err(format!(
                "eval pass rate {:.2} is below the requested floor {:.2}",
                report.totals.pass_rate, min
            )
            .into());
        }
    }

    Ok(())
}
