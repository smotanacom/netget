//! NetGet's Socket.IO client against python-socketio 5.17.0's server (independent,
//! unchanged, ASGI under uvicorn) over WebSocket and long-polling: the greeting, an emit with
//! an acknowledgement and the broadcast back, a server event the client acknowledges, a
//! namespace with auth, a refused namespace, and a server-side disconnect. Fails, never skips.
use crate::helpers::socketio::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

async fn find(
    state: &netget::state::app_state::AppState,
    owner: AccessLogOwner,
    kind: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows = logs(state, owner, kind, 1).await;
            if let Some(r) = rows.iter().find(|r| pred(&r.request)) {
                break r.request.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no matching {kind}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn client_talks_to_python_socketio_server() {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .arg("server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(60), lines.next_line())
            .await
            .expect("server did not start")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let port = first["port"].as_u64().unwrap();
    let state = state();
    for transport in ["websocket", "polling"] {
        let cid = client_in(&state, format!("127.0.0.1:{port}"), json!({"transport": transport, "namespaces": ["/", "/chat"], "auth": {"token": "secret"}})).await.unwrap();
        let owner = AccessLogOwner::Client(cid.as_u32());
        let connected = logs(&state, owner, "socketio_connected", 2).await;
        assert!(connected.iter().any(|c| c.request["namespace"] == "/chat"));
        let welcome = find(&state, owner, "socketio_event", |r| r["event"] == "welcome").await;
        assert_eq!(
            welcome["args"][0][0], "python server",
            "python-socketio sends a list as one argument"
        );
        let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(10));
        assert!(matches!(
            send(
                json!({"type":"socketio_emit","event":"chat message","args":[transport],"ack":true})
            )
            .await
            .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
        let ack = logs(&state, owner, "socketio_ack_received", 1).await;
        assert_eq!(ack[0].request["args"], json!(["delivered", transport]));
        let broadcast = find(&state, owner, "socketio_event", |r| {
            r["event"] == "chat message"
        })
        .await;
        assert_eq!(broadcast["args"], json!([["broadcast", transport]]));
        send(json!({"type":"socketio_emit","event":"chat message","args":["ns"],"namespace":"/chat","ack":true})).await.unwrap();
        let acks = logs(&state, owner, "socketio_ack_received", 2).await;
        assert_eq!(acks[1].request["args"], json!(["chat namespace", "ns"]));
        send(json!({"type":"socketio_emit","event":"ask"}))
            .await
            .unwrap();
        let question = find(&state, owner, "socketio_event", |r| {
            r["event"] == "question"
        })
        .await;
        let ack_id = question["ack_id"]
            .as_u64()
            .expect("python-socketio asked for an ack");
        send(json!({"type":"socketio_ack","ack_id":ack_id,"args":["yes", 42]}))
            .await
            .unwrap();
        let answer = find(&state, owner, "socketio_event", |r| {
            r["event"] == "answer received"
        })
        .await;
        assert_eq!(answer["args"], json!([["yes", 42]]));
        send(json!({"type":"socketio_emit","event":"kick"}))
            .await
            .unwrap();
        find(&state, owner, "socketio_disconnected", |_| true).await;
        state.remove_client(cid).await;
    }
    let refused = client_in(
        &state,
        format!("127.0.0.1:{port}"),
        json!({"namespaces": ["/chat"], "auth": {"token": "wrong"}}),
    )
    .await
    .unwrap();
    let err = logs(
        &state,
        AccessLogOwner::Client(refused.as_u32()),
        "socketio_connect_error",
        1,
    )
    .await;
    assert_eq!(err[0].request["message"], "not authorized");
    state.remove_client(refused).await;
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}
