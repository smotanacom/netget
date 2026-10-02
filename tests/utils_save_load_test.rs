use netget::utils::save_load::{is_actions_json, normalize_filename};

#[test]
fn test_normalize_filename() {
    assert_eq!(normalize_filename("myconfig"), "myconfig.netget");
    assert_eq!(normalize_filename("myconfig.netget"), "myconfig.netget");
    assert_eq!(normalize_filename("myconfig.txt"), "myconfig.netget");
    assert_eq!(normalize_filename("myconfig.json"), "myconfig.netget");
    assert_eq!(normalize_filename("my.config.txt"), "my.config.netget");
    assert_eq!(
        normalize_filename("configs/demo.json"),
        "configs/demo.netget"
    );
    assert_eq!(normalize_filename("configs/demo"), "configs/demo.netget");
    assert_eq!(normalize_filename("/tmp/demo.netget"), "/tmp/demo.netget");
    assert_eq!(normalize_filename("configs/.netget"), "configs/.netget");
    assert_eq!(
        normalize_filename(" ../configs/demo.txt "),
        "../configs/demo.netget"
    );
}

#[tokio::test]
async fn saved_instances_retain_handlers_and_feedback_in_the_requested_directory() {
    use netget::events::handler::EventHandler;
    use netget::state::{AppState, ClientId, ClientInstance, ServerId, ServerInstance};
    use netget::utils::save_load::{load_actions, save_all, save_client, save_server};
    use serde_json::json;

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let handlers = vec![json!({
        "event_pattern": "*",
        "handler": {"type": "static", "actions": []}
    })];
    let config = EventHandler::parse_event_handlers(handlers).unwrap();
    let expected_handlers = serde_json::to_value(&config.handlers).unwrap();
    let mut server = ServerInstance::new(ServerId::new(0), 12345, "tcp".into(), "server".into());
    server.event_handler_config = Some(config.clone());
    server.feedback_instructions = Some("server feedback".into());
    let server_id = state.add_server(server).await;
    let mut client = ClientInstance::new(
        ClientId::new(0),
        "127.0.0.1:12345".into(),
        "tcp".into(),
        "client".into(),
    );
    client.event_handler_config = Some(config);
    client.feedback_instructions = Some("client feedback".into());
    let client_id = state.add_client(client).await;
    let dir = tempfile::tempdir().unwrap();

    for (name, kind) in [
        ("server.json", "server"),
        ("client.json", "client"),
        ("all.json", "all"),
    ] {
        let input = dir.path().join(name);
        let filename = input.to_str().unwrap();
        let saved = match kind {
            "server" => save_server(&state, server_id, filename).await,
            "client" => save_client(&state, client_id, filename).await,
            _ => save_all(&state, filename).await,
        }
        .unwrap();
        assert_eq!(saved, input.with_extension("netget"));
        let actions = load_actions(filename).await.unwrap();
        assert_eq!(actions.len(), if kind == "all" { 2 } else { 1 });
        for action in actions {
            assert_eq!(action["event_handlers"], expected_handlers);
            let restored = EventHandler::parse_event_handlers(
                action["event_handlers"].as_array().unwrap().clone(),
            )
            .unwrap();
            assert!(restored.find_handler("any_event").is_some());
            let feedback = if action["type"] == "open_server" {
                "server feedback"
            } else {
                "client feedback"
            };
            assert_eq!(action["feedback_instructions"], feedback);
        }
    }
}

#[test]
fn test_is_actions_json() {
    // Valid actions JSON - {"actions": [...]} format
    assert!(is_actions_json(
        r#"{"actions":[{"type":"open_server","port":8080,"base_stack":"http","instruction":"test"}]}"#
    ));
    assert!(is_actions_json(
        r#"{"actions":[{"type":"show_message","message":"hello"}]}"#
    ));

    // Invalid - not wrapped in actions object
    assert!(!is_actions_json(r#"[{"type":"open_server"}]"#));

    // Invalid - wrong key name
    assert!(!is_actions_json(r#"{"action":[{"type":"open_server"}]}"#));

    // Invalid - empty actions array
    assert!(!is_actions_json(r#"{"actions":[]}"#));

    // Invalid - missing type field
    assert!(!is_actions_json(r#"{"actions":[{"port":8080}]}"#));

    // Invalid - not JSON
    assert!(!is_actions_json("hello world"));
    assert!(!is_actions_json("listen on port 80"));
}

#[tokio::test]
async fn session_relationships_and_tasks_restore_with_fresh_ids() {
    use netget::llm::OllamaClient;
    use netget::state::task::{ScheduledTask, TaskId, TaskScope};
    use netget::state::{AppState, ServerId, ServerInstance};
    use netget::utils::save_load::{load_actions, restore_session, save_all};
    use std::collections::BTreeMap;
    let original = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let from = original
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "tcp".into(),
            "source".into(),
        ))
        .await;
    let to = original
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "tcp".into(),
            "target".into(),
        ))
        .await;
    original
        .add_pipe(
            from,
            "tcp_data_received".into(),
            to,
            "send_tcp_data".into(),
            BTreeMap::from([("data".into(), "{data}".into())]),
        )
        .await
        .unwrap();
    original
        .add_task(
            ScheduledTask::new_one_shot(
                TaskId::new(0),
                "later".into(),
                TaskScope::Server(from),
                3600,
                "noop".into(),
                None,
            )
            .unwrap(),
        )
        .await;
    original
        .add_task(
            ScheduledTask::new_one_shot(
                TaskId::new(0),
                "global".into(),
                TaskScope::Global,
                3600,
                "noop".into(),
                None,
            )
            .unwrap(),
        )
        .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.netget");
    save_all(&original, path.to_str().unwrap()).await.unwrap();
    let actions = load_actions(path.to_str().unwrap()).await.unwrap();
    assert_eq!(actions[0]["type"], "restore_session");
    let restored = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    restored
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "tcp".into(),
            "unrelated".into(),
        ))
        .await;
    let llm = OllamaClient::new("http://127.0.0.1:1");
    restore_session(&restored, &llm, &actions[0]["session"])
        .await
        .unwrap();
    let servers = restored.get_all_servers().await;
    let restored_from = servers
        .iter()
        .find(|s| s.instruction == "source")
        .unwrap()
        .id;
    let restored_to = servers
        .iter()
        .find(|s| s.instruction == "target")
        .unwrap()
        .id;
    assert_ne!(restored_from, from);
    let pipes = restored.list_pipes().await;
    assert_eq!(
        (pipes[0].from, pipes[0].to),
        (restored_from.as_u32(), restored_to.as_u32())
    );
    let tasks = restored.get_all_tasks().await;
    assert_eq!(tasks.len(), 2);
    assert!(tasks
        .iter()
        .any(|t| matches!(t.scope, TaskScope::Server(id) if id == restored_from)));
    for server in servers {
        restored.remove_server(server.id).await;
    }
}

#[tokio::test]
async fn malformed_session_references_fail_before_starting_any_resource() {
    use netget::state::AppState;
    use netget::utils::save_load::restore_session;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let llm = netget::llm::OllamaClient::new("http://127.0.0.1:1");
    let invalid = serde_json::json!({"version":2,"resources":[],"pipes":[{
        "id":1,"from":1,"to":2,"on":"data","as":"send_tcp_data","map":{"data":"hello"}
    }]});
    assert!(restore_session(&state, &llm, &invalid).await.is_err());
    assert!(state.get_all_servers().await.is_empty());
    for action in [
        serde_json::json!(true),
        serde_json::json!([1]),
        serde_json::json!("not an action"),
        serde_json::Value::Null,
    ] {
        let invalid = serde_json::json!({"version":2,"resources":[{"key":1,"action":action}]});
        assert!(restore_session(&state, &llm, &invalid).await.is_err());
    }
    assert!(state.get_all_servers().await.is_empty());
}
