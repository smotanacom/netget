//! The run loop, and how this harness handles the fact that models are not
//! deterministic.
//!
//! # Why a rate and not a boolean
//!
//! NetGet passes exactly one option to its Ollama backend — `num_predict`
//! (`src/llm/ollama_client.rs`). There is no `temperature`, no `seed`, no
//! `top_p`, and no CLI flag that would set one. So the sampling temperature
//! here is whatever the model's own Modelfile says, typically 0.8, and **the
//! same instruction genuinely produces different actions run to run.**
//!
//! Given that, there were three options:
//!
//! 1. Report a boolean from one run. Rejected: a flaky eval reported as a
//!    boolean is worse than no eval, and this one would flip on protocols that
//!    are 60% fine.
//! 2. Pin the seed and report a boolean. **Not available without editing
//!    `src/`**, which this pass may not do. It is the right long-term answer and
//!    is written up as a recommendation in `EVAL_RESULTS.md`.
//! 3. Report a pass *rate* over N independent runs, publish every run's verdict,
//!    and let the reader see 2/3 rather than a rounded 0.67.
//!
//! This harness does (3). Every run gets a **fresh netget process and a fresh
//! server**, so runs are independent: no conversation history, no server memory
//! and no connection state carries between them. That costs a process start per
//! run — cheap, because a `--server`-direct start involves no model call at all —
//! and it buys the right to call the runs independent.
//!
//! N defaults to 3 (`NETGET_EVAL_RUNS`). Nightly uses 5. Below 3 the rate
//! carries no information; above 5 the wall clock stops being nightly-shaped.

#![allow(dead_code)]

use super::case::{EvalCase, Independence, ProbeKind};
use super::classify::{classify, Diagnosis};
use super::probe;
use crate::helpers::common::E2EResult;
use crate::helpers::llm_live::{live_model, LiveRequestTest};
use std::time::{Duration, Instant};

/// Default repetitions per case. Overridable with `NETGET_EVAL_RUNS`.
pub const DEFAULT_RUNS: usize = 3;

/// How long a third-party client may wait for the answer.
///
/// Measured rather than guessed: a network event carries a ~19,500-character
/// system prompt (the protocol's whole action vocabulary), and `llama3.1:8b`
/// took **88 seconds** to answer one on this machine. netget then retries a
/// malformed reply once, so a *failing* exchange is two of those. 240s covers
/// the retry; past it the client gives up and the run is recorded as
/// `no_wire_response`, which is what the peer would see too.
pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 240;

pub fn runs_per_case() -> usize {
    std::env::var("NETGET_EVAL_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_RUNS)
}

pub fn probe_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("NETGET_EVAL_PROBE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PROBE_TIMEOUT_SECS),
    )
}

/// Block until Ollama answers `/api/tags`, or give up after `budget`.
///
/// This exists because of a defect in the shared live-test helpers that this
/// pass may not edit, and the shape of it is worth recording.
/// `check_ollama_available` (`tests/helpers/netget.rs`) builds a fresh
/// `reqwest::Client` and gives `http://localhost:11434/api/tags` **2 seconds**;
/// `ensure_model_available` (`tests/helpers/llm_live.rs`) does the same with 5.
/// Both costs this repository has already documented land on exactly that call:
/// `Client::builder().build()` loads the platform root store from the macOS
/// keychain synchronously, and `localhost` resolves through mDNSResponder.
/// Worse, they run **immediately after the previous case's model call**, while
/// Ollama is still finishing it — so a run that did heavy work makes the next
/// run's availability check fail against a healthy server.
///
/// Measured in the second smoke run: two of five cases refused to start, three
/// attempts each, two seconds apart, with Ollama up and serving throughout.
///
/// So: one client, built once, with a real budget, polled until Ollama is
/// actually responsive. By the time the helpers run their 2-second check, it
/// answers instantly.
pub async fn wait_for_ollama_ready(budget: Duration) -> bool {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default()
    });
    let url =
        std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(resp) = client.get(format!("{}/api/tags", url)).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Protocol filter, e.g. `NETGET_EVAL_PROTOCOLS=http,dns`. Empty means all.
pub fn protocol_filter() -> Vec<String> {
    std::env::var("NETGET_EVAL_PROTOCOLS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// One repetition.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunRecord {
    pub run: usize,
    /// `pass`, `fail`, or `error` (the harness could not complete the run).
    pub verdict: &'static str,
    pub failure_mode: Option<String>,
    pub detail: Option<String>,
    /// The model's actual output, verbatim. The reason this file exists.
    pub model_output: Vec<String>,
    /// Action names netget would have executed had its response parser taken the
    /// first JSON value instead of requiring the whole reply to be one. Non-empty
    /// only for `valid_actions_rejected_as_unparseable`.
    pub recovered_actions: Vec<String>,
    pub client_command: String,
    pub client_output: String,
    pub client_exit: Option<i32>,
    pub client_timed_out: bool,
    pub elapsed_secs: f64,
}

/// Every repetition of one case, plus the score.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseResult {
    pub id: String,
    pub protocol: String,
    pub instruction: String,
    pub client: String,
    pub independence: String,
    pub expectation: String,
    pub note: Option<String>,
    pub attempts: usize,
    pub passes: usize,
    /// `passes / attempts`, or `null` when the case could not be attempted.
    pub pass_rate: Option<f64>,
    /// `attempted`, `no-client` (no installed client can drive it), or
    /// `client-missing` (the named binary is not on this machine).
    pub status: &'static str,
    pub status_reason: Option<String>,
    /// The mode that accounts for most misses, if any.
    pub dominant_failure: Option<String>,
    /// Failed runs in which the model had in fact produced executable actions,
    /// discarded only because the reply had text around the JSON. `passes +
    /// this` is what the score would be with a one-call fix to the parser.
    pub recoverable_runs: usize,
    pub runs: Vec<RunRecord>,
}

impl CaseResult {
    fn skipped(case: &EvalCase, status: &'static str, reason: String) -> Self {
        Self {
            id: case.id.to_string(),
            protocol: case.protocol.to_string(),
            instruction: case.instruction.to_string(),
            client: match &case.probe {
                ProbeKind::Command(p) => p.describe(),
                ProbeKind::Unavailable { .. } => "-".to_string(),
            },
            independence: match &case.probe {
                ProbeKind::Command(p) => p.independence.label().to_string(),
                ProbeKind::Unavailable { .. } => "none".to_string(),
            },
            expectation: case.expect.describe(),
            note: case.note.map(|s| s.to_string()),
            attempts: 0,
            passes: 0,
            pass_rate: None,
            status,
            status_reason: Some(reason),
            dominant_failure: None,
            recoverable_runs: 0,
            runs: Vec::new(),
        }
    }
}

/// Run one case `runs` times and score it.
pub async fn run_case(case: &EvalCase, runs: usize) -> CaseResult {
    let probe_spec = match &case.probe {
        ProbeKind::Unavailable { reason } => {
            println!("⊘ {} — no client: {}", case.id, reason);
            return CaseResult::skipped(case, "no-client", reason.to_string());
        }
        ProbeKind::Command(p) => p,
    };

    if !probe::binary_available(probe_spec.bin) {
        let reason = format!("{:?} is not installed on this machine", probe_spec.bin);
        println!("⊘ {} — {}", case.id, reason);
        return CaseResult::skipped(case, "client-missing", reason);
    }

    let mut records = Vec::new();
    for run in 1..=runs {
        println!("▶ {} run {}/{}", case.id, run, runs);
        records.push(run_once(case, probe_spec, run).await);
    }

    let passes = records.iter().filter(|r| r.verdict == "pass").count();
    let attempts = records.len();
    let dominant = dominant_failure(&records);
    let recoverable = records
        .iter()
        .filter(|r| r.verdict != "pass" && !r.recovered_actions.is_empty())
        .count();

    println!(
        "{} {} — {}/{} ({}){}",
        if passes == attempts { "✅" } else { "❌" },
        case.id,
        passes,
        attempts,
        probe_spec.independence.label(),
        dominant
            .as_ref()
            .map(|m| format!(" — {}", m))
            .unwrap_or_default()
    );

    CaseResult {
        id: case.id.to_string(),
        protocol: case.protocol.to_string(),
        instruction: case.instruction.to_string(),
        client: probe_spec.describe(),
        independence: probe_spec.independence.label().to_string(),
        expectation: case.expect.describe(),
        note: case.note.map(|s| s.to_string()),
        attempts,
        passes,
        pass_rate: Some(passes as f64 / attempts as f64),
        status: "attempted",
        status_reason: None,
        dominant_failure: dominant,
        recoverable_runs: recoverable,
        runs: records,
    }
}

fn dominant_failure(records: &[RunRecord]) -> Option<String> {
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for record in records {
        if let Some(mode) = &record.failure_mode {
            *counts.entry(mode.clone()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(mode, _)| mode)
}

/// One independent repetition: fresh netget process, fresh server, one probe.
async fn run_once(case: &EvalCase, probe_spec: &super::case::Probe, run: usize) -> RunRecord {
    let started = Instant::now();

    // Starting is retried, and the reason is a defect in the shared harness
    // rather than flakiness being papered over. `check_ollama_available`
    // (`tests/helpers/netget.rs`) is hardcoded to `http://localhost:11434`, gives
    // it a **2-second** timeout, and builds a fresh `reqwest::Client` for it — so
    // it pays both macOS costs this repo has already documented: `localhost`
    // resolves through mDNSResponder (measured at 8.25s under concurrency) and
    // `Client::builder().build()` loads the platform root store from the keychain
    // synchronously. Five of eleven cases in the first smoke run were refused by
    // that 2-second check against an Ollama that was up and serving. The fix
    // belongs in that helper, which this pass may not edit; until then a start is
    // worth three tries before it counts as a real failure.
    const START_ATTEMPTS: usize = 3;
    let mut start_error = String::new();
    let mut started_server = None;
    for attempt in 1..=START_ATTEMPTS {
        // Make sure Ollama is responsive before the helpers' 2-second check
        // runs. See `wait_for_ollama_ready`.
        if !wait_for_ollama_ready(Duration::from_secs(120)).await {
            start_error = "Ollama did not answer /api/tags within 120s".to_string();
            continue;
        }
        let mut builder = LiveRequestTest::new(case.protocol, case.instruction);
        if let Some(params) = &case.server_params {
            builder = builder.server_params(params.clone());
        }
        match builder.start().await {
            Ok(s) => {
                started_server = Some(s);
                break;
            }
            Err(e) => {
                start_error = e.to_string();
                if attempt < START_ATTEMPTS {
                    println!(
                        "  … start attempt {} failed ({}), retrying",
                        attempt, start_error
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    let server = match started_server {
        Some(s) => s,
        None => {
            return RunRecord {
                run,
                verdict: "error",
                failure_mode: Some("server_start_failed".to_string()),
                detail: Some(format!(
                    "netget did not start a {} server in {} attempts: {}",
                    case.protocol, START_ATTEMPTS, start_error
                )),
                model_output: Vec::new(),
                recovered_actions: Vec::new(),
                client_command: probe_spec.describe(),
                client_output: String::new(),
                client_exit: None,
                client_timed_out: false,
                elapsed_secs: started.elapsed().as_secs_f64(),
            };
        }
    };

    let outcome = probe::run(probe_spec, server.port, probe_timeout()).await;

    // One-way protocols (syslog) write nothing back, so the client exits the
    // instant the datagram is sent — long before the model has been asked. For
    // those the observable is netget's own log, and reading it the moment the
    // client returns would race the model call every time. Wait for the needles
    // instead of sleeping on a guess.
    if !case.expect.server_log_all_of.is_empty() {
        for needle in &case.expect.server_log_all_of {
            // Returns quietly on timeout; the check below is what asserts.
            let _ = server
                .instance
                .wait_for_log(needle, probe_timeout().as_secs())
                .await;
        }
    }

    // Read the log *after* the probe, so it contains this exchange.
    let log = server.instance.get_output().await;
    let log_text = log.join("\n");
    let _ = server.finish().await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            return RunRecord {
                run,
                verdict: "error",
                failure_mode: Some("client_spawn_failed".to_string()),
                detail: Some(e),
                model_output: Vec::new(),
                recovered_actions: Vec::new(),
                client_command: probe_spec.describe(),
                client_output: String::new(),
                client_exit: None,
                client_timed_out: false,
                elapsed_secs: started.elapsed().as_secs_f64(),
            };
        }
    };

    let combined = outcome.combined();
    match case.expect.check(&combined, &log_text) {
        Ok(()) => RunRecord {
            run,
            verdict: "pass",
            failure_mode: None,
            detail: None,
            model_output: super::classify::model_output(&log),
            recovered_actions: Vec::new(),
            client_command: outcome.command.clone(),
            client_output: clip(&combined),
            client_exit: outcome.exit_code,
            client_timed_out: outcome.timed_out,
            elapsed_secs: started.elapsed().as_secs_f64(),
        },
        Err(why) => {
            let Diagnosis {
                mode,
                detail,
                evidence,
                recovered_actions,
            } = classify(&log, &outcome, &why);
            RunRecord {
                run,
                verdict: "fail",
                failure_mode: Some(mode.to_string()),
                detail: Some(detail),
                model_output: evidence,
                recovered_actions,
                client_command: outcome.command.clone(),
                client_output: clip(&combined),
                client_exit: outcome.exit_code,
                client_timed_out: outcome.timed_out,
                elapsed_secs: started.elapsed().as_secs_f64(),
            }
        }
    }
}

/// Keep the results file readable: a client that dumps a whole page is common
/// and only the head of it ever diagnoses anything.
fn clip(text: &str) -> String {
    const MAX: usize = 2000;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX).collect();
    format!("{}… (truncated)", head)
}

/// Run every case the filter selects.
pub async fn run_suite(cases: &[EvalCase]) -> E2EResult<Vec<CaseResult>> {
    let filter = protocol_filter();
    let runs = runs_per_case();
    let selected: Vec<&EvalCase> = cases
        .iter()
        .filter(|c| filter.is_empty() || filter.contains(&c.protocol.to_lowercase()))
        .collect();

    println!(
        "🧪 real-model eval: {} case(s), {} run(s) each, model {}",
        selected.len(),
        runs,
        live_model()
    );

    let model = live_model();
    let mut results = Vec::new();
    for (done, case) in selected.iter().enumerate() {
        results.push(run_case(case, runs).await);

        // Publish after every case, not only at the end.
        //
        // A full sweep is ~48 cases at several minutes each, because a reply
        // netget cannot parse costs two model calls rather than one. Writing the
        // artefacts only on completion means a run that is interrupted — or that
        // someone simply cannot wait out — leaves nothing at all, and a partial
        // measurement is worth far more than no measurement. Every write is a
        // complete, self-consistent report of the cases finished so far.
        let partial = super::report::build(&model, runs, results.clone());
        if let Err(e) = super::report::write(&partial) {
            eprintln!("⚠ could not write interim results: {}", e);
        }
        println!(
            "   … {}/{} cases done, {}/{} runs passed so far",
            done + 1,
            selected.len(),
            partial.totals.runs_passed,
            partial.totals.runs_total
        );
    }
    Ok(results)
}

/// Independence rollup for the summary: how much of the score rests on a real
/// protocol client rather than a byte pipe.
pub fn independence_of(label: &str) -> Independence {
    if label == Independence::ProtocolClient.label() {
        Independence::ProtocolClient
    } else {
        Independence::GenericTransport
    }
}
