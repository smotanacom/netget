//! The ZeroMQ client against NetGet's own server: REQ to REP (and the refusal of a second
//! send before the reply), DEALER to ROUTER with an Identity, PUSH to PULL, and actions a
//! socket type cannot perform rejected before anything is sent.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const ECHO_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nif e['socket_type']=='PULL':\n  a={'type':'zmq_ignore'}\nelse:\n  a={'type':'zmq_reply','frames':['ECHO']+e['frames']+[e.get('peer_identity') or '-']}\nprint(json.dumps({'actions':[a]}))";

async fn netget_server(socket_type: &str) -> (AppState, netget::state::ServerId, String) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "zeromq".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Answer messages".into()),
        startup_params: Some(json!({"socket_type": socket_type})),
        event_handlers: Some(vec![json!({"event_pattern":"zmq_message","handler":{"type":"script","language":"python","code":ECHO_SCRIPT}})]),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, format!("127.0.0.1:{}", addr.port()))
}

pub async fn client(remote: String, params: Value, ready: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "zeromq".into(),
        remote_addr: Some(remote),
        instruction: Some("Talk to the socket".into()),
        startup_params: Some(params),
        event_handlers: Some(vec![
            json!({"event_pattern":"zmq_connected","handler":{"type":"static","actions":ready}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(entry) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .find(|e| e.contains(needle))
            {
                break entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no client event containing {needle:?}"))
}

pub async fn send(state: &AppState, id: ClientId, action: Value) -> ClientSendOutcome {
    state
        .send_to_client(id, action, Duration::from_secs(20))
        .await
        .unwrap()
}

#[tokio::test]
async fn req_dealer_and_push_against_netget() {
    let (server_state, server_id, addr) = netget_server("rep").await;
    let (state, id) = client(
        addr.clone(),
        json!({}),
        json!([{"type":"zmq_send","frames":["hello","there"]}]),
    )
    .await;
    wait_log(&state, id, r#""frames":["ECHO","hello","there","-"]"#).await;
    // A REQ socket may not send twice before its reply: the second is refused locally.
    let first = send(&state, id, json!({"type":"zmq_send","frames":["one"]})).await;
    let second = send(&state, id, json!({"type":"zmq_send","frames":["two"]})).await;
    assert!(matches!(first, ClientSendOutcome::Sent { .. }), "{first:?}");
    assert!(
        format!("{second:?}").contains("must receive its reply"),
        "{second:?}"
    );
    wait_log(&state, id, r#""frames":["ECHO","one","-"]"#).await;
    let sub = send(&state, id, json!({"type":"zmq_subscribe","topic":"x"})).await;
    assert!(format!("{sub:?}").contains("only a SUB socket"), "{sub:?}");
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;

    let (server_state, server_id, addr) = netget_server("router").await;
    let (state, id) = client(
        addr,
        json!({"socket_type":"dealer","identity":"dealer-9"}),
        json!([{"type":"zmq_send","frames":["a"]},{"type":"zmq_send","frames":["b"]}]),
    )
    .await;
    wait_log(&state, id, r#""frames":["ECHO","a","dealer-9"]"#).await;
    wait_log(&state, id, r#""frames":["ECHO","b","dealer-9"]"#).await;
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;

    let (server_state, server_id, addr) = netget_server("pull").await;
    let (state, id) = client(addr, json!({"socket_type":"push"}), json!([])).await;
    let sent = send(
        &state,
        id,
        json!({"type":"zmq_send","frames":["ff01"],"encoding":"hex"}),
    )
    .await;
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let seen = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if server_state
                .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .any(|e| e.contains(r#""frames":["ff01"]"#) && e.contains(r#""encoding":"hex""#))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        seen.is_ok(),
        "the PULL server's handler saw the binary frame as hex"
    );
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
