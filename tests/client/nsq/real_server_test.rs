use super::common::*;
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::json;
use std::time::Duration;
async fn daemon() -> crate::helpers::E2EResult<RealServer> {
    RealServer::builder(
        "nsqd",
        InstallHint {
            brew: "nsq",
            apt: "nsq (install official nsq v1.3.0 release binaries if unavailable)",
        },
    )
    .args([
        "--tcp-address=127.0.0.1:{port}",
        "--http-address=127.0.0.1:{port1}",
        "--data-path={dir}",
        "--broadcast-address=127.0.0.1",
    ])
    .extra_ports(1)
    .ready_when_log_matches("TCP: listening on")
    .start()
    .await
}
#[tokio::test]
async fn independent_nsqd_publish_subscribe_readiness_fin_req_touch_and_close(
) -> crate::helpers::E2EResult<()> {
    let server = daemon().await?;
    let state = state();
    let subscriber = connected_client(&state, server.addr()).await;
    assert_eq!(
        request(
            &state,
            subscriber,
            json!({"operation":"subscribe","topic":"jobs","channel":"workers"})
        )
        .await["status"],
        "OK"
    );
    let publisher = connected_client(&state, server.addr()).await;
    let body = "first ✓\nPUB should remain message text";
    assert_eq!(
        request(
            &state,
            publisher,
            json!({"operation":"publish","topic":"jobs","body":body})
        )
        .await["command"],
        "PUB"
    );
    assert_eq!(
        request(
            &state,
            publisher,
            json!({"operation":"publish_many","topic":"jobs","messages":["second","third"]})
        )
        .await["command"],
        "MPUB"
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Client(subscriber.as_u32())),
                None
            )
            .await
            .iter()
            .any(|e| e.event_type == "nsq_message"),
        "SUB begins with RDY0"
    );
    send(&state, subscriber, json!({"operation":"ready","count":1})).await;
    let (first_id, first) = event(&state, subscriber, "nsq_message", 0).await;
    assert_eq!(first["body"], body);
    assert_eq!(first["attempts"], 1);
    send(
        &state,
        subscriber,
        json!({"operation":"touch","message_id":first["message_id"]}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Client(subscriber.as_u32())),
                None
            )
            .await
            .iter()
            .any(|e| e.event_type == "nsq_message" && e.id > first_id),
        "RDY1 limits concurrent messages"
    );
    send(
        &state,
        subscriber,
        json!({"operation":"finish","message_id":first["message_id"]}),
    )
    .await;
    // nsqd keeps RDY1 as a concurrency ceiling: FIN admits the next without a new RDY.
    let (second_id, second) = event(&state, subscriber, "nsq_message", first_id).await;
    assert_eq!(second["body"], "second");
    send(&state, subscriber, json!({"operation":"ready","count":0})).await;
    send(
        &state,
        subscriber,
        json!({"operation":"requeue","message_id":second["message_id"],"delay_ms":50}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Client(subscriber.as_u32())),
                None
            )
            .await
            .iter()
            .any(|e| e.event_type == "nsq_message" && e.id > second_id),
        "RDY0 pauses redelivery"
    );
    send(&state, subscriber, json!({"operation":"ready","count":1})).await;
    let (next_id, next) = event(&state, subscriber, "nsq_message", second_id).await;
    send(
        &state,
        subscriber,
        json!({"operation":"finish","message_id":next["message_id"]}),
    )
    .await;
    let (last_id, last) = event(&state, subscriber, "nsq_message", next_id).await;
    // NSQ deliberately does not promise requeue ordering relative to pending messages.
    let (redelivery, third) = if next["body"] == "second" {
        (&next, &last)
    } else {
        (&last, &next)
    };
    assert_eq!(third["body"], "third");
    assert_eq!(redelivery["body"], "second");
    assert_eq!(redelivery["message_id"], second["message_id"]);
    assert_eq!(redelivery["attempts"], 2);
    send(
        &state,
        subscriber,
        json!({"operation":"finish","message_id":last["message_id"]}),
    )
    .await;
    assert_eq!(
        request(
            &state,
            publisher,
            json!({"operation":"publish_deferred","topic":"jobs","body":"later","delay_ms":50})
        )
        .await["command"],
        "DPUB"
    );
    let (_, later) = event(&state, subscriber, "nsq_message", last_id).await;
    assert_eq!(later["body"], "later");
    send(
        &state,
        subscriber,
        json!({"operation":"finish","message_id":later["message_id"]}),
    )
    .await;
    let after = latest(&state, subscriber).await;
    send(
        &state,
        subscriber,
        json!({"operation":"touch","message_id":"ffffffffffffffff"}),
    )
    .await;
    let (_, error) = event(&state, subscriber, "nsq_error", after).await;
    assert_eq!(error["code"], "E_TOUCH_FAILED");
    assert_eq!(error["fatal"], false);
    let close = request(&state, subscriber, json!({"operation":"close"})).await;
    assert_eq!(close["status"], "CLOSE_WAIT");
    state.remove_client(publisher).await;
    state.remove_client(subscriber).await;
    Ok(())
}
#[tokio::test]
async fn model_actions_publish_to_independent_nsqd_and_continue_on_response(
) -> crate::helpers::E2EResult<()> {
    let server = daemon().await?;
    let state = state();
    let subscriber = connected_client(&state, server.addr()).await;
    request(
        &state,
        subscriber,
        json!({"operation":"subscribe","topic":"model","channel":"proof"}),
    )
    .await;
    send(&state, subscriber, json!({"operation":"ready","count":2})).await;
    let publisher=client(&state,server.addr(),vec![
        static_handler("nsq_connected",json!([{"type":"nsq_request","operation":"publish","topic":"model","body":"handler"}])),
        json!({"event_pattern":"nsq_response","handler":{"type":"script","language":"python","code":"import json, sys\nevent = json.load(sys.stdin)['event']\njson.dump({'actions': [{'type':'nsq_request','operation':'publish','topic':'model','body':'followup'}] if event['request']['body'] == 'handler' else []}, sys.stdout)"}}),
        static_handler("*",json!([]))]).await;
    let (first_id, first) = event(&state, subscriber, "nsq_message", 0).await;
    assert_eq!(first["body"], "handler");
    assert_eq!(
        event(&state, subscriber, "nsq_message", first_id).await.1["body"],
        "followup"
    );
    state.remove_client(publisher).await;
    state.remove_client(subscriber).await;
    Ok(())
}
#[tokio::test]
async fn independent_nsqd_heartbeats_keep_manual_connected_event_alive(
) -> crate::helpers::E2EResult<()> {
    let server = daemon().await?;
    let state = state();
    let id = client(
        &state,
        server.addr(),
        vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":60}})],
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(matches!(
        state.get_client(id).await.unwrap().status,
        netget::state::ClientStatus::Connected
    ));
    assert!(state.has_client_handle(id).await);
    state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
        .await
        .unwrap();
    state.remove_client(id).await;
    Ok(())
}
#[tokio::test]
async fn mocked_model_writes_to_real_nsqd_and_memory_is_available_on_followup(
) -> crate::helpers::E2EResult<()> {
    use crate::helpers::{mock_builder::MockLlmBuilder, mock_ollama::MockOllamaServer};
    let server = daemon().await?;
    let config = MockLlmBuilder::new()
        .on_event("nsq_connected")
        .respond_with_actions(json!([
            {"type":"set_memory","value":"published first NSQ message"},
            {"type":"nsq_request","operation":"publish","topic":"llm","body":"model chose this"}
        ]))
        .expect_calls(1)
        .and()
        .on_event("nsq_response")
        .and_prompt_containing("published first NSQ message")
        .respond_with_actions(json!([]))
        .expect_calls(1)
        .and()
        .build();
    let mock = MockOllamaServer::start(config).await?;
    let state = netget::state::AppState::new_with_options(false, mock.base_url());
    state.set_ollama_model(Some("mock".into())).await;
    let subscriber = connected_client(&state, server.addr()).await;
    request(
        &state,
        subscriber,
        json!({"operation":"subscribe","topic":"llm","channel":"proof"}),
    )
    .await;
    send(&state, subscriber, json!({"operation":"ready","count":1})).await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let publisher = netget::cli::management::ClientForm {
        protocol: "nsq".into(),
        remote_addr: Some(server.addr()),
        instruction: Some("Publish a chosen message; record progress in memory".into()),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new(mock.base_url()), tx)
    .await?;
    assert_eq!(
        event(&state, subscriber, "nsq_message", 0).await.1["body"],
        "model chose this"
    );
    event(&state, publisher, "nsq_response", 0).await;
    assert_eq!(
        state.get_memory_for_client(publisher).await.unwrap(),
        "published first NSQ message"
    );
    assert_eq!(mock.call_count().await, 2);
    mock.verify_calls().await?;
    state.remove_client(publisher).await;
    state.remove_client(subscriber).await;
    Ok(())
}
