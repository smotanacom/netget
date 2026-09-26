//! The eval's own probes and expectations, run against a **mocked** model.
//!
//! The eval scores a real model, so a defect in the harness scores as a defect
//! in the model — and two of them did, for whole protocols, before anything
//! here existed:
//!
//! - **beanstalkd**, 0/15 `event_never_reached_model`. The greenstalk probe was
//!   a Rust string built with `\` continuations, which drop the next line's
//!   leading whitespace; Python raised `IndentationError` before connecting.
//! - **gemini** and **bolt**, 0/15 each `client_left_before_model_answered`.
//!   Both clients write a warning to stderr before the exchange (Python's
//!   `CryptographyDeprecationWarning`, the JVM's `ThreadPriorityPolicy`), so
//!   the probe's two-second idle settle killed them during the model call.
//!
//! Each test here takes one published case by id — its instruction, its probe
//! and its `Expect`, unchanged — and puts a mocked model behind it that answers
//! the case's event **correctly** and only after [`MODEL_LATENCY`], which is
//! longer than the probe's settle. It then requires three things, each of which
//! one of the defects above broke:
//!
//! 1. the model was asked for the event the instruction depends on
//!    (`expect_calls` on the event rule — the beanstalkd defect fails here);
//! 2. the probe was still there when the slow answer arrived;
//! 3. the case's own expectation accepts that correct answer (the gemini and
//!    bolt defects fail here and in 2).
//!
//! So a case that passes here and scores 0 in the eval is a finding about the
//! model or the descriptions, not about the harness.
//!
//! These tests drive the same real clients the eval does and **fail** when one
//! is missing, as the eval itself reports `client-missing` rather than
//! skipping.
//!
//! Run with, e.g.:
//!   ./cargo-isolated.sh test --no-default-features --features beanstalkd --test eval -- probe_check --test-threads=100

#![allow(dead_code)]

use super::case::ProbeKind;
use super::probe;
use super::suites::all_cases;
use crate::helpers::common::E2EResult;
use crate::helpers::mock_builder::MockLlmBuilder;
use crate::helpers::{start_netget_server, NetGetConfig};
use std::time::Duration;

/// How long the mocked model takes to answer each event: longer than the
/// probe's idle settle (two seconds after the first byte), shorter than any
/// client's own timeout.
pub const MODEL_LATENCY: Duration = Duration::from_secs(5);

/// Run published case `id` against a mocked model. `answer` adds the rules for
/// the case's events; each must use `.after_delay(MODEL_LATENCY)` and pin its
/// call count, which is what proves the event reached the model.
pub async fn check_case<F>(id: &'static str, answer: F) -> E2EResult<()>
where
    F: FnOnce(MockLlmBuilder) -> MockLlmBuilder,
{
    let case = all_cases()
        .into_iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no eval case {id:?} in this build"));
    let ProbeKind::Command(probe_spec) = case.probe.clone() else {
        panic!("eval case {id} has no probe to check");
    };
    let protocol = case.protocol;
    let instruction = case.instruction;

    let config = NetGetConfig::new(format!(
        "listen on port {{AVAILABLE_PORT}} via {protocol}. Eval probe check."
    ))
    .with_log_level("debug")
    // `run-eval.sh` runs this binary with NETGET_USE_OLLAMA=1; these checks are about the
    // mock's answers, never a real model's.
    .with_forced_mock()
    .with_mock(move |mock| {
        let mock = mock
            .on_instruction_containing(format!("via {protocol}"))
            .respond_with_actions(serde_json::json!([{
                "type": "open_server",
                "port": 0,
                "base_stack": protocol,
                "instruction": instruction
            }]))
            .expect_calls(1)
            .and();
        answer(mock)
    });

    let server = start_netget_server(config).await?;
    let outcome = probe::run(&probe_spec, server.port, Duration::from_secs(120))
        .await
        .map_err(|e| format!("{id}: {e}"))?;
    let output = outcome.combined();
    println!(
        "--- {id}: {} (exit {:?}, {:.1}s) ---\n{output}",
        outcome.command,
        outcome.exit_code,
        outcome.elapsed.as_secs_f64()
    );

    server.wait_for_mocks(30).await;
    // First: was the model asked at all? A probe that never reaches the server
    // fails here, naming the rule that went unanswered.
    server.verify_mocks().await?;
    assert!(
        !outcome.timed_out,
        "{id}: the probe was still running after 120s"
    );
    // Then: did the probe wait for the slow answer, and does the case's own
    // expectation accept a correct one?
    if let Err(why) = case.expect.check(&output, "") {
        panic!(
            "{id}: a mocked model answered correctly after {MODEL_LATENCY:?} and the case still \
             fails — {why}. That is a harness defect, and the eval would score it against the \
             model. Client output:\n{output}"
        );
    }
    server.stop().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// beanstalkd — greenstalk
// ---------------------------------------------------------------------------

#[cfg(feature = "beanstalkd")]
#[tokio::test]
async fn beanstalkd_accept_a_job_reaches_the_model() -> E2EResult<()> {
    check_case("beanstalkd/accept-a-job", |mock| {
        mock.on_event("beanstalkd_put")
            .respond_with_actions(
                serde_json::json!([{"type": "insert_beanstalkd_job", "job_id": 100}]),
            )
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "beanstalkd")]
#[tokio::test]
async fn beanstalkd_hand_out_a_job_reaches_the_model() -> E2EResult<()> {
    check_case("beanstalkd/hand-out-a-job", |mock| {
        mock.on_event("beanstalkd_reserve")
            .respond_with_actions(serde_json::json!([{
                "type": "reserve_beanstalkd_job",
                "job_id": 7,
                "body": "resize photo.jpg to 640 wide"
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "beanstalkd")]
#[tokio::test]
async fn beanstalkd_queue_statistics_reaches_the_model() -> E2EResult<()> {
    check_case("beanstalkd/queue-statistics", |mock| {
        mock.on_event("beanstalkd_stats")
            .respond_with_actions(serde_json::json!([{
                "type": "send_beanstalkd_stats",
                "stats": {"current-jobs-ready": 5, "current-jobs-buried": 2, "version": "1.13"}
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

// ---------------------------------------------------------------------------
// gemini — ignition
// ---------------------------------------------------------------------------

#[cfg(feature = "gemini")]
#[tokio::test]
async fn gemini_home_page_waits_for_the_model() -> E2EResult<()> {
    check_case("gemini/home-page", |mock| {
        mock.on_event("gemini_request")
            .respond_with_actions(serde_json::json!([{
                "type": "send_gemtext",
                "lines": [
                    {"type": "heading1", "text": "Welcome to the NetGet capsule"},
                    {"type": "link", "url": "/about", "text": "About"}
                ]
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "gemini")]
#[tokio::test]
async fn gemini_ask_for_input_waits_for_the_model() -> E2EResult<()> {
    check_case("gemini/ask-for-input", |mock| {
        mock.on_event("gemini_request")
            .respond_with_actions(serde_json::json!([{
                "type": "send_gemini_response",
                "status": 10,
                "meta": "What is your name?"
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "gemini")]
#[tokio::test]
async fn gemini_not_found_waits_for_the_model() -> E2EResult<()> {
    check_case("gemini/not-found", |mock| {
        mock.on_event("gemini_request")
            .respond_with_actions(serde_json::json!([{
                "type": "send_gemini_response",
                "status": 51,
                "meta": "Not found"
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

// ---------------------------------------------------------------------------
// docker — the docker CLI
// ---------------------------------------------------------------------------

#[cfg(feature = "docker")]
#[tokio::test]
async fn docker_ps_running_container_waits_for_the_model() -> E2EResult<()> {
    check_case("docker/ps-running-container", |mock| {
        mock.on_event("docker_api_request")
            .and_event_data_contains("resource", "containers")
            .respond_with_actions(serde_json::json!([{
                "type": "send_docker_containers",
                "containers": [{
                    "names": ["eval-web"], "image": "nginx:1.27", "state": "running",
                    "ports": [{"private_port": 80, "public_port": 8080, "type": "tcp"}]
                }]
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "docker")]
#[tokio::test]
async fn docker_ps_all_includes_stopped_waits_for_the_model() -> E2EResult<()> {
    check_case("docker/ps-all-includes-stopped", |mock| {
        mock.on_event("docker_api_request")
            .and_event_data_contains("resource", "containers")
            .respond_with_actions(serde_json::json!([{
                "type": "send_docker_containers",
                "containers": [
                    {"names": ["eval-api"], "image": "api:2", "state": "running"},
                    {"names": ["eval-migrate"], "image": "api:2", "state": "exited",
                     "exit_code": 0}
                ]
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "docker")]
#[tokio::test]
async fn docker_inspect_missing_waits_for_the_model() -> E2EResult<()> {
    check_case("docker/inspect-missing", |mock| {
        mock.on_event("docker_api_request")
            .and_event_data_contains("resource", "container")
            .respond_with_actions(serde_json::json!([{
                "type": "send_docker_error",
                "status": 404,
                "message": "No such container: eval-ghost"
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
            // Before giving up on a name, the CLI reads /info to decide whether to try the
            // swarm object types.
            .on_event("docker_api_request")
            .and_event_data_contains("resource", "info")
            .respond_with_actions(serde_json::json!([{"type": "send_docker_info"}]))
            .after_delay(MODEL_LATENCY)
            .expect_at_most(1)
            .and()
    })
    .await
}

// ---------------------------------------------------------------------------
// bolt — cypher-shell
// ---------------------------------------------------------------------------

/// The login every bolt case needs first. One or two: the Java driver's pool
/// sometimes opens a second connection, and each logs in.
#[cfg(feature = "bolt")]
fn accept_bolt_login(mock: MockLlmBuilder) -> MockLlmBuilder {
    mock.on_event("bolt_authenticate")
        .respond_with_actions(serde_json::json!([{"type": "accept_bolt_login"}]))
        .after_delay(MODEL_LATENCY)
        .expect_at_least(1)
        .expect_at_most(2)
        .and()
}

#[cfg(feature = "bolt")]
#[tokio::test]
async fn bolt_people_by_name_waits_for_the_model() -> E2EResult<()> {
    check_case("bolt/people-by-name", |mock| {
        accept_bolt_login(mock)
            .on_event("bolt_query")
            .respond_with_actions(serde_json::json!([{
                "type": "send_bolt_records",
                "fields": ["name"],
                "records": [["Ada"], ["Grace"], ["Linus"]]
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "bolt")]
#[tokio::test]
async fn bolt_count_waits_for_the_model() -> E2EResult<()> {
    check_case("bolt/count", |mock| {
        accept_bolt_login(mock)
            .on_event("bolt_query")
            .respond_with_actions(serde_json::json!([{
                "type": "send_bolt_records",
                "fields": ["movies"],
                "records": [[42]]
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}

#[cfg(feature = "bolt")]
#[tokio::test]
async fn bolt_syntax_error_waits_for_the_model() -> E2EResult<()> {
    check_case("bolt/syntax-error", |mock| {
        accept_bolt_login(mock)
            .on_event("bolt_query")
            .respond_with_actions(serde_json::json!([{
                "type": "send_bolt_failure",
                "code": "Neo.ClientError.Statement.SyntaxError",
                "message": "Invalid input 'SELECT': expected 'MATCH' (line 1, column 1 (offset: 0))"
            }]))
            .after_delay(MODEL_LATENCY)
            .expect_calls(1)
            .and()
    })
    .await
}
