//! Pure scoring regressions. No subprocess, NetGet binary, network, or model calls.
mod helpers {
    pub mod child_guard;
}
#[path = "eval/case.rs"]
mod case;
#[path = "eval/classify.rs"]
mod classify;
#[path = "eval/probe.rs"]
mod probe;
#[path = "eval/scoring.rs"]
mod scoring;

fn outcome(output: &str, truncated: bool) -> probe::ProbeOutcome {
    probe::ProbeOutcome {
        stdout: output.into(),
        stderr: String::new(),
        exit_code: Some(0),
        timed_out: false,
        output_truncated: truncated,
        elapsed: std::time::Duration::ZERO,
        command: "fixture-only".into(),
    }
}

#[test]
fn truncated_output_is_a_harness_error_even_when_the_retained_prefix_matches() {
    let expect = case::Expect::contains(&["expected answer"]);
    let log = vec![
        "Executing action send_data".into(),
        "unrelated diagnostic".into(),
    ];
    for output in ["expected answer", "different answer"] {
        let score = scoring::score_probe(&expect, "tcp", "reply", &log, &outcome(output, true));
        assert_eq!(score.verdict, "error");
        assert_eq!(score.failure_mode.as_deref(), Some("probe_output_limit"));
        assert!(score.detail.unwrap().contains("HARNESS:"));
        assert!(score.recovered_actions.is_empty());
        assert_eq!(score.model_output, vec!["Executing action send_data"]);
    }
}

#[test]
fn invalid_expectation_regex_is_a_harness_error_with_existing_evidence() {
    let expect = case::Expect {
        regex: Some("[".into()),
        ..Default::default()
    };
    let log = vec!["Malformed response (raw): original fixture reply".into()];
    let score = scoring::score_probe(&expect, "tcp", "reply", &log, &outcome("answer", false));
    assert_eq!(score.verdict, "error");
    assert_eq!(score.failure_mode.as_deref(), Some("harness_error"));
    assert!(score.detail.unwrap().contains("bad regex"));
    assert_eq!(score.model_output, log);
}

#[test]
fn complete_output_still_distinguishes_passes_from_model_failures() {
    let expect = case::Expect::contains(&["expected answer"]);
    let passed = scoring::score_probe(
        &expect,
        "tcp",
        "reply",
        &[],
        &outcome("expected answer", false),
    );
    assert_eq!(passed.verdict, "pass");
    assert!(passed.failure_mode.is_none());
    let failed = scoring::score_probe(
        &expect,
        "tcp",
        "reply",
        &[],
        &outcome("different answer", false),
    );
    assert_eq!(failed.verdict, "fail");
    assert_ne!(failed.failure_mode.as_deref(), Some("probe_output_limit"));
}

#[test]
fn action_names_in_rejected_output_cannot_satisfy_execution_expectations() {
    let expect = case::Expect {
        executed_actions_all_of: vec!["send_data".into()],
        ..Default::default()
    };
    let rejected = vec!["Malformed response (raw): send_data".into()];
    assert_eq!(
        scoring::score_probe(&expect, "tcp", "reply", &rejected, &outcome("", false)).verdict,
        "fail"
    );
    let executed = vec!["INFO Executing action send_data".into()];
    assert_eq!(
        scoring::score_probe(&expect, "tcp", "reply", &executed, &outcome("", false)).verdict,
        "pass"
    );
}
