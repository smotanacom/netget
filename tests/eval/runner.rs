//! The run loop, and how this harness handles the fact that models are not
//! deterministic.
//!
//! # A pinned seed, and still a rate
//!
//! Every run passes `--llm-seed` (default [`DEFAULT_SEED`], `NETGET_EVAL_SEED`;
//! `none` sends nothing) and, when `NETGET_EVAL_TEMPERATURE` is set,
//! `--llm-temperature`. Ollama's sampler then draws the same tokens for the same
//! prompt, so a case whose prompt is identical run to run answers identically.
//!
//! **That is weaker than it sounds, and the report measures by how much.** The
//! prompt is not identical run to run: the event data carries an ephemeral
//! client port, a connection id, and for DNS/LDAP-style protocols a random
//! query or message id, and any one differing token changes every token the
//! seed draws after it. So the harness keeps N independent runs and reports,
//! per case, whether the runs **agreed** — the same verdict, and the same
//! executed actions — rather than assuming they would.
//!
//! Every run gets a **fresh netget process and a fresh server**, so runs are
//! independent: no conversation history, no server memory and no connection
//! state carries between them. That costs a process start per run — cheap,
//! because a `--server`-direct start involves no model call at all — and it
//! buys the right to call the runs independent.
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

/// The sampler seed every run passes unless `NETGET_EVAL_SEED` says otherwise.
/// Fixed, so two sweeps of the same tree are comparable.
pub const DEFAULT_SEED: u64 = 42;

/// `--llm-seed` for every run: `NETGET_EVAL_SEED`, [`DEFAULT_SEED`] when unset,
/// and no seed at all for `none` (the unpinned behaviour, for comparison).
pub fn eval_seed() -> Option<u64> {
    match std::env::var("NETGET_EVAL_SEED") {
        Err(_) => Some(DEFAULT_SEED),
        Ok(v) if v.trim().is_empty() || v.trim().eq_ignore_ascii_case("none") => None,
        Ok(v) => Some(
            v.trim()
                .parse()
                .unwrap_or_else(|_| panic!("NETGET_EVAL_SEED={v:?} is not a u64 or `none`")),
        ),
    }
}

/// `--llm-temperature` for every run, from `NETGET_EVAL_TEMPERATURE`. Unset by
/// default: the model's own temperature applies and only the seed is pinned.
pub fn eval_temperature() -> Option<f32> {
    std::env::var("NETGET_EVAL_TEMPERATURE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| {
            v.trim()
                .parse()
                .unwrap_or_else(|_| panic!("NETGET_EVAL_TEMPERATURE={v:?} is not a number"))
        })
}

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
/// The shared helpers now build one client and allow a real budget
/// (`crate::helpers::common::{ollama_http_client, OLLAMA_PROBE_TIMEOUT}`), so
/// this is no longer working around them — but it is still the right thing for a
/// **sweep**, for a different reason than the one it was written for.
///
/// A gate asks "is Ollama there". A sweep needs "is Ollama ready *now*", and the
/// two differ because each case starts on the heels of the previous case's model
/// call. Ollama is routinely unresponsive for far longer than any single request
/// bound while it loads or unloads a model — measured in the second smoke run as
/// two of five cases refusing to start, three attempts each, with Ollama up and
/// serving throughout. Polling against a budget measured in minutes is the only
/// form of the question a long run can ask, and a sweep that gives up at step 1
/// measures nothing at all.
pub async fn wait_for_ollama_ready(budget: Duration) -> bool {
    let client = crate::helpers::common::ollama_http_client();
    let url = crate::helpers::common::ollama_base_url();
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(Ok(resp)) = tokio::time::timeout(
            crate::helpers::common::OLLAMA_PROBE_TIMEOUT,
            client.get(format!("{}/api/tags", url)).send(),
        )
        .await
        {
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
    /// What netget executed, one entry per `Executing action` line with the log
    /// prefix (timestamp, level) removed — the comparable part, for agreement.
    pub executed_actions: Vec<String>,
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
    /// Every run reached the same verdict. `null` when fewer than two runs.
    pub verdicts_agree: Option<bool>,
    /// Every run executed exactly the same actions, byte for byte. Stricter than
    /// `verdicts_agree`: a random query id in the answer breaks it by design.
    pub actions_agree: Option<bool>,
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
            verdicts_agree: None,
            actions_agree: None,
            runs: Vec::new(),
        }
    }
}

/// Whether every run agrees on `key`. `None` below two runs, where agreement
/// is vacuous and reporting it as `true` would overstate reproducibility.
fn all_agree<T: PartialEq>(records: &[RunRecord], key: impl Fn(&RunRecord) -> T) -> Option<bool> {
    if records.len() < 2 {
        return None;
    }
    let first = key(&records[0]);
    Some(records[1..].iter().all(|r| key(r) == first))
}

/// The part of an `Executing action` line that is the action itself.
fn executed_actions(log: &[String]) -> Vec<String> {
    log.iter()
        .filter_map(|l| {
            l.find("Executing action")
                .map(|i| l[i..].trim().to_string())
        })
        .collect()
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
        verdicts_agree: all_agree(&records, |r| r.verdict),
        actions_agree: all_agree(&records, |r| r.executed_actions.clone()),
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

    // Starting is retried because a sweep starts each case on the heels of the
    // previous case's model call, and Ollama swapping models is not an outage.
    // The shared helper's own bound is no longer the reason: it was two seconds
    // against a freshly built client — five of eleven cases in the first smoke
    // run were refused by it against an Ollama that was up and serving — and is
    // now `OLLAMA_PROBE_TIMEOUT` against one shared client.
    const START_ATTEMPTS: usize = 3;
    let mut start_error = String::new();
    let mut started_server = None;
    for attempt in 1..=START_ATTEMPTS {
        // Make sure Ollama is responsive before the helper's own check runs.
        // See `wait_for_ollama_ready`.
        if !wait_for_ollama_ready(Duration::from_secs(120)).await {
            start_error = "Ollama did not answer /api/tags within 120s".to_string();
            continue;
        }
        let mut builder = LiveRequestTest::new(case.protocol, case.instruction)
            .sampling(eval_seed(), eval_temperature());
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
                executed_actions: Vec::new(),
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
    if !case.expect.executed_actions_all_of.is_empty() {
        // Wait for an execution, not merely for the name: netget logs a rejected
        // reply verbatim, so the name appears whether or not anything ran.
        let _ = server
            .instance
            .wait_for_log("Executing action", probe_timeout().as_secs())
            .await;
        for needle in &case.expect.executed_actions_all_of {
            // Returns quietly on timeout; the check below is what asserts.
            let _ = server
                .instance
                .wait_for_log(needle, probe_timeout().as_secs())
                .await;
        }
    }

    // Read the log *after* the probe, so it contains this exchange.
    let log = server.instance.get_output().await;
    // Only the lines that prove netget *ran* something. Handing `check` the
    // whole log let an `executed_action` expectation match netget's own dump of
    // a **rejected** reply, which is how syslog scored a false 3/3.
    let executed_text = log
        .iter()
        .filter(|l| l.contains("Executing action"))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
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
                executed_actions: Vec::new(),
                client_command: probe_spec.describe(),
                client_output: String::new(),
                client_exit: None,
                client_timed_out: false,
                elapsed_secs: started.elapsed().as_secs_f64(),
            };
        }
    };

    let combined = outcome.combined();
    match case.expect.check(&combined, &executed_text) {
        Ok(()) => RunRecord {
            run,
            verdict: "pass",
            failure_mode: None,
            detail: None,
            model_output: super::classify::model_output(&log),
            recovered_actions: Vec::new(),
            executed_actions: executed_actions(&log),
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
            } = classify(case.protocol, case.instruction, &log, &outcome, &why);
            RunRecord {
                run,
                verdict: "fail",
                failure_mode: Some(mode.to_string()),
                detail: Some(detail),
                model_output: evidence,
                recovered_actions,
                executed_actions: executed_actions(&log),
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
        "🧪 real-model eval: {} case(s), {} run(s) each, model {}, seed {:?}, temperature {:?}",
        selected.len(),
        runs,
        live_model(),
        eval_seed(),
        eval_temperature()
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
