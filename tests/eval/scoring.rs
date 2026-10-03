//! Pure outcome classification, shared by the live runner and CPU-only regressions.

use super::case::Expect;
use super::classify::{classify, model_output};
use super::probe::ProbeOutcome;

pub struct ProbeScore {
    pub verdict: &'static str,
    pub failure_mode: Option<String>,
    pub detail: Option<String>,
    pub model_output: Vec<String>,
    pub recovered_actions: Vec<String>,
}

/// Refuse incomplete harness evidence before checking expectations or diagnosing
/// model behavior. A matching prefix of truncated output is not a passing run.
pub fn score_probe(
    expect: &Expect,
    protocol: &str,
    instruction: &str,
    log: &[String],
    outcome: &ProbeOutcome,
) -> ProbeScore {
    if outcome.output_truncated {
        return ProbeScore {
            verdict: "error",
            failure_mode: Some("probe_output_limit".into()),
            detail: Some("HARNESS: probe output exceeded the capture limit".into()),
            model_output: model_output(log),
            recovered_actions: Vec::new(),
        };
    }

    // Only executed-action lines can satisfy action expectations. A rejected
    // model response mentioning the same action name is not execution evidence.
    let executed_text = log
        .iter()
        .filter(|line| line.contains("Executing action"))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    match expect.check(
        &outcome.combined(),
        &executed_text,
        outcome.exit_code,
        outcome.timed_out,
    ) {
        Ok(()) => ProbeScore {
            verdict: "pass",
            failure_mode: None,
            detail: None,
            model_output: model_output(log),
            recovered_actions: Vec::new(),
        },
        Err(why) => {
            let diagnosis = classify(protocol, instruction, log, outcome, &why);
            ProbeScore {
                verdict: if why.starts_with("HARNESS:") {
                    "error"
                } else {
                    "fail"
                },
                failure_mode: Some(diagnosis.mode.into()),
                detail: Some(diagnosis.detail),
                model_output: diagnosis.evidence,
                recovered_actions: diagnosis.recovered_actions,
            }
        }
    }
}
