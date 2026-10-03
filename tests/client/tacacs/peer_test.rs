use crate::helpers::tacacs::{accounting, authentication, authorization, client, logs, Peer};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use std::time::Duration;
#[tokio::test]
async fn native_client_interoperates_with_unmodified_sdk_receiver_all_selected_operations() {
    let mut peer = Peer::start().await;
    let (state, id) = client(peer.addr.to_string(), None, None).await;
    let owner = AccessLogOwner::Client(id.as_u32());
    for (index, method) in ["ascii", "pap"].into_iter().enumerate() {
        let result = state
            .send_to_client(id, authentication(method), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(matches!(result, ClientSendOutcome::Executed { .. }));
        let rows = logs(&state, owner, "tacacs_authentication_result", index + 1).await;
        assert_eq!(rows[index].request["authenticated"], true);
        assert_eq!(
            rows[index].request["reply"]["server_message"],
            "peer authentication"
        );
        assert!(rows[index].request["request"].get("password").is_none());
    }
    let mut bad = authentication("pap");
    bad["password"] = "wrong".into();
    state
        .send_to_client(id, bad, Duration::from_secs(10))
        .await
        .unwrap();
    let rows = logs(&state, owner, "tacacs_authentication_result", 3).await;
    assert_eq!(rows[2].request["authenticated"], false);
    state
        .send_to_client(id, authorization(), Duration::from_secs(10))
        .await
        .unwrap();
    let rows = logs(&state, owner, "tacacs_authorization_result", 1).await;
    assert_eq!(rows[0].request["authorized"], true);
    assert_eq!(rows[0].request["reply"]["arguments"][0]["value"], "15");
    assert_eq!(
        rows[0].request["ignored_optional_arguments"][0]["name"],
        "audit"
    );
    assert_eq!(rows[0].request["device_policy_applied"], false);
    for (index, kind) in ["start", "stop", "watchdog", "update"]
        .into_iter()
        .enumerate()
    {
        state
            .send_to_client(id, accounting(kind), Duration::from_secs(10))
            .await
            .unwrap();
        let rows = logs(&state, owner, "tacacs_accounting_result", index + 1).await;
        assert_eq!(rows[index].request["recorded_by_peer"], true);
        assert_eq!(rows[index].request["durable_storage_confirmed"], false);
        let recorded = peer.recorded().await;
        assert_eq!(recorded["recorded"], true);
        assert_eq!(recorded["username"], "alice");
        assert_eq!(
            recorded["arguments"],
            serde_json::json!(["task_id=42", "start_time=1700000000"])
        );
        assert_eq!(recorded["record_type"], [2, 4, 8, 10][index]);
        assert_eq!(recorded["port"], "tty1");
        assert_eq!(recorded["remote_address"], "192.0.2.10");
    }
    let injected = logs(&state, owner, "injected_action", 8).await;
    assert!(injected
        .iter()
        .all(|e| !e.request.to_string().contains("correct")));
    assert_eq!(injected[0].request["password"], "<redacted>");
    state.remove_client(id).await;
    peer.stop().await;
}
