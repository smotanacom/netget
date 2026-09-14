//! The bridge LLM backend: requests go to whoever drains the bridge, answers come back as
//! the model's.
//!
//! This is the backend the browser build runs on (`crates/netget-web` drains the bridge into
//! the page's JavaScript), but nothing about it is browser-specific, so the mapping from a
//! `ConversationHandler` round-trip to a `BridgeRequest` and from a `BridgeReply` back to
//! parsed actions is pinned here, natively, through the same public API the scheduled-task
//! runner uses. The wasm bundle's own end-to-end check is `web/test/smoke.mjs`.

use std::sync::Arc;
use std::time::Duration;

use netget::llm::{
    ActionDefinition, BridgeReply, BridgeRequestKind, ConversationHandler, LlmBridge, OllamaClient,
    Parameter, RateLimiter, RateLimiterConfig, RequestSource,
};
use netget::state::app_state::WebSearchMode;
use serde_json::json;

fn send_tcp_data() -> ActionDefinition {
    ActionDefinition {
        name: "send_tcp_data".to_string(),
        description: "Send data to the peer".to_string(),
        parameters: vec![Parameter {
            name: "data".to_string(),
            type_hint: "string".to_string(),
            description: "Payload".to_string(),
            required: true,
        }],
        example: json!({"type": "send_tcp_data", "data": "hi"}),
        log_template: None,
    }
}

fn conversation(client: OllamaClient) -> ConversationHandler {
    let mut conversation = ConversationHandler::new(
        "system prompt under test".to_string(),
        Arc::new(client),
        "page-model".to_string(),
        RateLimiter::new(RateLimiterConfig::default()),
        RequestSource::Network,
    );
    conversation.add_user_message("hello from the peer".to_string());
    conversation
}

#[tokio::test]
async fn a_generate_request_carries_the_whole_prompt_and_its_text_answer_is_parsed_as_actions() {
    let (bridge, mut rx) = LlmBridge::new();
    bridge.set_models(vec!["page-model".to_string()]);
    let client = OllamaClient::new_bridge(bridge.clone(), Duration::from_secs(5));
    assert_eq!(client.backend_type(), "bridge");
    assert_eq!(client.list_models().await.unwrap(), vec!["page-model"]);

    // The host: check the request is the full prompt, then answer with an action envelope.
    let host = tokio::spawn(async move {
        let req = rx.recv().await.expect("the bridge delivers the request");
        assert_eq!(req.kind, BridgeRequestKind::Generate);
        assert_eq!(req.model, "page-model");
        assert!(
            req.tools.is_empty(),
            "the generate path embeds actions in the prompt"
        );
        let prompt: String = req.messages.iter().map(|m| m.content.as_str()).collect();
        assert!(
            prompt.contains("system prompt under test"),
            "system prompt missing"
        );
        assert!(prompt.contains("hello from the peer"), "user turn missing");
        req.reply
            .send(Ok(BridgeReply {
                content: Some(
                    r#"{"actions":[{"type":"send_tcp_data","data":"HELLO FROM THE PEER"}]}"#
                        .to_string(),
                ),
                tool_calls: vec![],
                prompt_tokens: 3,
                completion_tokens: 2,
            }))
            .expect("the client is waiting");
    });

    let actions = conversation(client)
        .generate_with_tools_and_retry(None, WebSearchMode::Off, vec![send_tcp_data()])
        .await
        .expect("the host's answer parses as actions");
    host.await.unwrap();

    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["type"], "send_tcp_data");
    assert_eq!(actions[0]["data"], "HELLO FROM THE PEER");
}

#[tokio::test]
async fn a_chat_request_with_native_tools_maps_the_hosts_tool_calls_to_actions() {
    let (bridge, mut rx) = LlmBridge::new();
    let client = OllamaClient::new_bridge(bridge, Duration::from_secs(5));

    let host = tokio::spawn(async move {
        let req = rx.recv().await.expect("the bridge delivers the request");
        assert_eq!(req.kind, BridgeRequestKind::Chat);
        assert_eq!(
            req.tools.len(),
            1,
            "the tool schema travels with a chat request"
        );
        assert_eq!(req.tools[0]["function"]["name"], "send_tcp_data");
        assert_eq!(req.messages[0].role, "system");
        req.reply
            .send(Ok(BridgeReply {
                content: None,
                tool_calls: vec![netget::llm::bridge::BridgeToolCall {
                    id: None,
                    name: "send_tcp_data".to_string(),
                    arguments: json!({"data": "from a tool call"}),
                }],
                prompt_tokens: 0,
                completion_tokens: 0,
            }))
            .expect("the client is waiting");
    });

    let actions = conversation(client)
        .with_native_tools(&[send_tcp_data()])
        .generate_with_tools_and_retry(None, WebSearchMode::Off, vec![send_tcp_data()])
        .await
        .expect("a tool call is an action");
    host.await.unwrap();

    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["type"], "send_tcp_data");
    assert_eq!(actions[0]["data"], "from a tool call");
}

#[tokio::test]
async fn a_request_the_host_drops_fails_the_call_rather_than_hanging() {
    let (bridge, mut rx) = LlmBridge::new();
    let client = OllamaClient::new_bridge(bridge, Duration::from_secs(5));

    // Drop every request unanswered; the conversation retries once, then gives up.
    let host = tokio::spawn(async move {
        let mut seen = 0;
        while let Some(req) = rx.recv().await {
            seen += 1;
            drop(req);
        }
        seen
    });

    let err = conversation(client)
        .generate_with_tools_and_retry(None, WebSearchMode::Off, vec![send_tcp_data()])
        .await
        .expect_err("no answer is a failure");
    let text = format!("{err:#}");
    assert!(
        text.contains("dropped"),
        "the error names the cause: {text}"
    );
    drop(host);
}

#[tokio::test]
async fn an_error_from_the_host_is_the_models_error_not_a_transport_fault() {
    let (bridge, mut rx) = LlmBridge::new();
    let client = OllamaClient::new_bridge(bridge, Duration::from_secs(5));
    let breaker = client.circuit_breaker().clone();

    let host = tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let _ = req.reply.send(Err("the visitor refused".to_string()));
        }
    });

    let err = conversation(client)
        .generate_with_tools_and_retry(None, WebSearchMode::Off, vec![send_tcp_data()])
        .await
        .expect_err("a refusal is a failure");
    let text = format!("{err:#}");
    assert!(text.contains("the visitor refused"), "{text}");
    // A slow or unwilling host is not a backend outage: the breaker must stay closed so
    // the next request is still attempted.
    assert_eq!(breaker.status().consecutive_failures, 0);
    drop(host);
}
