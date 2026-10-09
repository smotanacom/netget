use crate::helpers::gnmi_peer as peer;
use netget::state::{client_handles::ClientSendOutcome, AppState, ClientId};
use serde_json::{json, Value};
async fn queued(state: &AppState, id: ClientId, action: Value) {
    assert!(matches!(
        peer::send(state, id, action).await.unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
}
fn get(id: u32, name: &str) -> Value {
    json!({"type":"gnmi_get","call_id":id,"request":{"path":[peer::path(name)],"encoding":"PROTO"}})
}
fn subscribe(id: u32, mode: &str, name: &str) -> Value {
    json!({"type":"gnmi_subscribe","call_id":id,"request":{"mode":mode,"subscription":[{"path":peer::path(name)}],"encoding":"PROTO"}})
}
#[tokio::test]
async fn independent_public_sdk_unary_typed_values_set_acknowledgement_and_gzip() {
    let sdk = peer::Peer::server(false).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(&state, sdk.port, peer::wait_handlers(), json!({}))
        .await
        .unwrap();
    for (call, mut action) in [
        (1, json!({"type":"gnmi_capabilities","call_id":1})),
        (2, get(2, "system")),
        (
            3,
            json!({"type":"gnmi_set","call_id":3,"request":{"delete":[peer::path("old")],"replace":[{"path":peer::path("new"),"value":{"kind":"int","value":i64::MIN.to_string()}}],"update":[{"path":peer::path("counter"),"value":{"kind":"uint","value":u64::MAX.to_string()}}]}}),
        ),
        (4, get(4, "denied")),
    ] {
        action["gzip"] = json!(true);
        queued(&state, id, action).await;
        assert_eq!(
            peer::log(&state, id, "gnmi_client_ended", call).await["code"],
            if call == 4 { 7 } else { 0 }
        );
    }
    assert_eq!(
        peer::log(&state, id, "gnmi_client_response", 2).await["response"]["notification"][0]
            ["update"][0]["value"],
        json!({"kind":"uint","value":"42"})
    );
    assert_eq!(
        peer::log(&state, id, "gnmi_client_response", 3).await["response"]["response"][2]
            ["operation"],
        "UPDATE"
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn native_pair_once_poll_stream_sync_and_responsive_cancellation() {
    let state = peer::state().await;
    let (server, port) = peer::server(&state, peer::handlers(), json!({}))
        .await
        .unwrap();
    let id = peer::client_id(&state, port, peer::wait_handlers(), json!({}))
        .await
        .unwrap();
    queued(&state, id, subscribe(1, "ONCE", "system")).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 1).await["code"],
        0
    );
    queued(&state, id, subscribe(2, "POLL", "system")).await;
    peer::log(&state, id, "gnmi_client_sync", 2).await;
    queued(&state, id, json!({"type":"gnmi_poll","call_id":2})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let logs = state
                .list_access_logs_for(
                    Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                    None,
                )
                .await;
            if logs.iter().any(|l| {
                l.event_type == "gnmi_client_sync"
                    && l.request["call_id"] == 2
                    && l.request["sequence"] == 4
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    queued(&state, id, json!({"type":"gnmi_cancel","call_id":2})).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 2).await["code"],
        1
    );
    queued(&state, id, subscribe(3, "STREAM", "system")).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 3).await["code"],
        0
    );
    state.remove_client(id).await;
    state.remove_server(server).await.unwrap();
}
