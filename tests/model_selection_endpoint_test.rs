//! Model auto-selection must ask the endpoint this process was pointed at.
//!
//! `ensure_model_selected` is the last-resort path: no `--model` was given and none was
//! persisted, so NetGet picks one off whatever backend it is talking to. It used to pick it off
//! a **hardcoded** `http://localhost:11434` whatever `--ollama-url` said, behind a comment
//! claiming the function is "typically called from interactive TUI mode where ollama_url should
//! already be validated during startup".
//!
//! That was false for three shipped paths, each of which sets the model only
//! `if let Some(model) = configured_model`: `--mcp` / `--mcp-http` (`src/mcp_stdio/tools.rs`),
//! `netget --client <proto> --connect <addr>` (`run_client`, which deliberately skips
//! `select_or_validate_model` so a fully scripted client needs no backend), and the dashboard,
//! which stores `None` when `resolve_startup_model` returns an empty name. On any of them,
//! `--ollama-url http://gpu-box:11434` with no `--model` auto-selected against localhost — so
//! either every request failed closed while a healthy backend sat idle, or a model name was
//! taken off a local Ollama and sent to a remote endpoint that does not have it.
//!
//! # Why this counts `/api/tags` rather than asserting on the returned name
//!
//! The failure's signature is that the configured endpoint receives **no request at all** — not
//! a failing one, not a 404. A test that only checked the selected model name could pass by
//! coincidence on a developer machine that happens to run an Ollama advertising a similar
//! model, and would prove nothing about *which host was asked*. Counting hits on the mock is
//! the only assertion that separates "asked the right host and it said no" from "asked a
//! different host".

mod helpers;

use helpers::mock_builder::MockLlmBuilder;
use helpers::mock_ollama::MockOllamaServer;

/// With no model configured, selection queries the endpoint it was handed.
#[tokio::test]
async fn auto_selection_queries_the_configured_endpoint() {
    let mock = MockOllamaServer::start(MockLlmBuilder::new().build())
        .await
        .expect("mock ollama");

    assert_eq!(
        mock.tags_request_count(),
        0,
        "nothing has asked the mock for its models yet"
    );

    let model = netget::llm::ensure_model_selected(None, &mock.base_url())
        .await
        .expect("auto-selection must succeed against an endpoint advertising models");

    assert_eq!(
        mock.tags_request_count(),
        1,
        "auto-selection must ask the CONFIGURED endpoint which models it has. A zero here means \
         it went somewhere else — which is the whole bug: the operator's backend never sees a \
         request, so its logs show nothing to explain the failure."
    );
    assert!(
        !model.is_empty(),
        "a model advertised by the configured endpoint must be chosen"
    );
}

/// A model that is already configured is used as-is; no endpoint is contacted.
///
/// This is the other half of the contract, and it is what keeps the fix cheap: the overwhelming
/// majority of calls take this branch, so threading the endpoint through costs nothing at
/// runtime.
#[tokio::test]
async fn a_configured_model_short_circuits_the_query() {
    let mock = MockOllamaServer::start(MockLlmBuilder::new().build())
        .await
        .expect("mock ollama");

    let model =
        netget::llm::ensure_model_selected(Some("pinned-model".to_string()), &mock.base_url())
            .await
            .expect("a configured model needs no lookup");

    assert_eq!(model, "pinned-model");
    assert_eq!(
        mock.tags_request_count(),
        0,
        "a configured model must not provoke a lookup"
    );
}

/// An unreachable endpoint is an error, not a silent fallback to localhost.
///
/// Port 1 on loopback has nothing bound. Before the fix this call would have consulted
/// `localhost:11434` instead and could have *succeeded* on a machine running Ollama — a
/// silent redirection to a backend the operator never asked for.
#[tokio::test]
async fn an_unreachable_endpoint_fails_rather_than_falling_back() {
    let err = netget::llm::ensure_model_selected(None, "http://127.0.0.1:1")
        .await
        .expect_err("an endpoint with nothing listening cannot supply a model");

    let text = format!("{:#}", err);
    assert!(
        text.contains("127.0.0.1:1") || text.to_lowercase().contains("connect"),
        "the error must name the endpoint that failed, not a default one: {}",
        text
    );
}
