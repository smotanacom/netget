use super::common::*;
use netget::llm::actions::protocol_trait::Protocol;
use serde_json::json;
#[tokio::test]
async fn advertised_static_and_script_examples_run_against_netget_server() {
    let state = super::server_common::new_state().await;
    let (server,port,_)=super::server_common::start(&state,vec![super::server_common::accept_logins(),json!({"event_pattern":"bolt_query","handler":{"type":"static","actions":[{"type":"send_bolt_records","fields":["answer"],"records":[[42]]}]}})],None).await;
    let protocol = netget::client::bolt::BoltClientProtocol::new();
    let examples = protocol.get_startup_examples();
    for example in [&examples.static_mode, &examples.script_mode] {
        let handlers = example["event_handlers"].as_array().unwrap().clone();
        let id = client(
            &state,
            format!("bolt://127.0.0.1:{port}"),
            json!({}),
            handlers,
        )
        .await;
        let page = event(&state, id, "bolt_result_page", 0).await.1;
        assert_eq!(page["fields"], json!(["answer"]));
        assert_eq!(page["records"], json!([[42]]));
        assert_eq!(page["has_more"], false);
        state.remove_client(id).await;
    }
    state.remove_server(server).await;
}
#[tokio::test]
async fn pair_script_graph_values_and_private_parameter_results_preserve_field_order() {
    let state = super::server_common::new_state().await;
    let (server, port, _) = super::server_common::start(
        &state,
        vec![
            super::server_common::accept_logins(),
            super::server_common::graph_handler(),
        ],
        None,
    )
    .await;
    let id = client(
        &state,
        format!("127.0.0.1:{port}"),
        json!({"username":"neo4j","password":"pair-private"}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    event(&state, id, "bolt_connected", 0).await;
    receipt(
        &state,
        id,
        json!({"type":"bolt_run","query":"MATCH p RETURN p"}),
        "bolt_query_started",
    )
    .await;
    let page = receipt(
        &state,
        id,
        json!({"type":"bolt_pull","n":1}),
        "bolt_result_page",
    )
    .await;
    assert_eq!(
        page["records"][0][0]["$path"]["nodes"][0]["$node"]["properties"]["name"],
        "Alice"
    );
    assert_eq!(
        page["records"][0][0]["$path"]["relationships"][0]["$unbound_relationship"]
            ["relationship_type"],
        "ACTED_IN"
    );
    receipt(&state,id,json!({"type":"bolt_run","query":"RETURN $x","parameters":{"x":{"ordinary":"pair-private"}}}),"bolt_query_started").await;
    let page = receipt(
        &state,
        id,
        json!({"type":"bolt_pull","n":1}),
        "bolt_result_page",
    )
    .await;
    assert_eq!(page["records"], json!([[{"ordinary":"<redacted>"}]]));
    state.remove_client(id).await;
    state.remove_server(server).await;
}
#[tokio::test]
async fn mocked_model_login_query_pull_use_common_memory_and_private_diagnostics(
) -> crate::helpers::E2EResult<()> {
    use crate::helpers::{mock_builder::MockLlmBuilder, mock_ollama::MockOllamaServer};
    let peer = MockPeer::start(8, "normal").await;
    let config=MockLlmBuilder::new().on_event("bolt_connected").respond_with_actions(json!([{ "type":"set_memory","value":"typed Bolt model session"},{"type":"bolt_login","username":"neo4j","password":"model-private"}])).expect_calls(1).and()
        .on_event("bolt_authentication").and_prompt_containing("typed Bolt model session").respond_with_actions(json!([{"type":"bolt_run","query":"RETURN 1"}])).expect_calls(1).and()
        .on_event("bolt_query_started").and_prompt_containing("typed Bolt model session").respond_with_actions(json!([{"type":"bolt_pull","n":3}])).expect_calls(1).and()
        .on_event("bolt_result_page").respond_with_actions(json!([])).expect_calls(1).and().build();
    let mock = MockOllamaServer::start(config).await?;
    let state = netget::state::AppState::new_with_options(false, mock.base_url());
    state.set_ollama_model(Some("mock".into())).await;
    let (tx, mut logs) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ClientForm {
        protocol: "bolt".into(),
        remote_addr: Some(peer.address.clone()),
        instruction: Some("Run a typed Bolt query and keep common memory".into()),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new(mock.base_url()), tx)
    .await?;
    let page = event(&state, id, "bolt_result_page", 0).await.1;
    assert_eq!(page["records"], json!([[1], [2], [3]]));
    assert_eq!(
        state.get_memory_for_client(id).await.as_deref(),
        Some("typed Bolt model session")
    );
    // Event logging precedes handler completion; wait on the independently counted model call.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while mock.call_count().await < 4 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    mock.verify_calls().await?;
    let mut diagnostics = String::new();
    while let Ok(line) = logs.try_recv() {
        diagnostics.push_str(&line)
    }
    assert!(!diagnostics.contains("model-private"));
    let access = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    assert!(!serde_json::to_string(&access)
        .unwrap()
        .contains("model-private"));
    state.remove_client(id).await;
    peer.stop().await;
    Ok(())
}
#[tokio::test]
async fn handler_followup_chain_is_bounded_after_four_native_acknowledgements() {
    let peer = MockPeer::start(8, "normal").await;
    let state = state();
    let id = client(
        &state,
        peer.address.clone(),
        json!({"username":"neo4j","password":"private"}),
        vec![
            static_handler("bolt_connected", json!([{"type":"bolt_reset"}])),
            static_handler("bolt_session_result", json!([{"type":"bolt_reset"}])),
            static_handler("*", json!([])),
        ],
    )
    .await;
    let mut after = 0;
    for _ in 0..4 {
        after = event(&state, id, "bolt_session_result", after).await.0;
    }
    // A later externally injected operation establishes an ordered receipt after handler refusal.
    let before = peer.seen.lock().unwrap().len();
    receipt(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 1"}),
        "bolt_query_started",
    )
    .await;
    assert_eq!(before, 6);
    let resets = peer
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|v| {
            matches!(
                v,
                netget::server::bolt::packstream::Value::Struct { tag: 0x0f, .. }
            )
        })
        .count();
    assert_eq!(resets, 4);
    state.remove_client(id).await;
    peer.stop().await;
}
