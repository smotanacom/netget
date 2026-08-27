//! What a Spark client gets when the LLM backend fails: a JSON error object with a 5xx.
//!
//! Spark's success responses are bare JSON arrays, so the dangerous failure is answering
//! `200 []` — a valid "no applications/jobs" result a client cannot distinguish from a backend
//! that never ran. This pins the LLM-failure path to a 5xx JSON *object* with an `error` field,
//! structurally distinct from any success array.

#![cfg(all(test, feature = "spark"))]

use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};
use serde_json::Value;
use std::time::Duration;

#[tokio::test]
async fn test_spark_answers_error_when_llm_fails() -> E2EResult<()> {
    let config =
        NetGetConfig::new_no_scripts("Open an Apache Spark REST API on port {AVAILABLE_PORT}")
            .with_mock(|mock| {
                mock.on_instruction_containing("Apache Spark REST API")
                    .respond_with_actions(serde_json::json!([{
                        "type": "open_server", "port": 0, "base_stack": "spark",
                        "instruction": "Spark monitoring API"
                    }]))
                    .expect_calls(1)
                    .and()
                // No rule for the spark_request event -> the mock 500s -> call_llm errors.
            });

    let server = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = reqwest::Client::new();
    let response = tokio::time::timeout(
        Duration::from_secs(25),
        client
            .get(format!(
                "http://127.0.0.1:{}/api/v1/applications",
                server.port
            ))
            .send(),
    )
    .await
    .map_err(|_| {
        "No Spark response within 25s - the server went silent on LLM failure, which is the exact \
         defect this test exists to catch"
    })??;

    let status = response.status().as_u16();
    let text = response.text().await?;
    println!("Spark -> {status} {text}");

    assert!(
        (500..600).contains(&status),
        "expected a 5xx rather than a success-shaped empty array: {status}"
    );
    let body: Value = serde_json::from_str(&text)?;
    assert!(
        !body.is_array(),
        "a failure must be a JSON error object, never a (possibly-empty) success array: {text}"
    );
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("netget"),
        "the error should name the source of the failure: {text}"
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
