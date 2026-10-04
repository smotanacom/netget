//! NetGet's A2A client against an a2a-sdk 1.2.1 agent — independent, unchanged — served with
//! the SDK's own Starlette routes: card, direct message, streamed task, GetTask, a working task
//! and its cancellation, and a not-found error. Fails, never skips, when absent.
use crate::helpers::a2a::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test(flavor = "multi_thread")]
async fn client_runs_messages_streams_and_tasks_against_a2a_sdk_agent() {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let port = first["port"].as_u64().unwrap();
    let state = state();
    let cid = client_in(&state, format!("127.0.0.1:{port}"), json!({}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "a2a_connected", 1).await;
    assert_eq!(
        (
            connected[0].request["name"].as_str(),
            connected[0].request["streaming"].as_bool()
        ),
        (Some("Python Echo"), Some(true))
    );
    let send = |a: Value| {
        let state = &state;
        async move {
            state
                .send_to_client(cid, a, Duration::from_secs(20))
                .await
                .unwrap()
        }
    };
    assert!(matches!(
        send(json!({"type":"a2a_send_message","text":"hello agent"})).await,
        ClientSendOutcome::Sent { .. }
    ));
    assert!(matches!(
        send(json!({"type":"a2a_send_message","text":"please make a task","stream":true})).await,
        ClientSendOutcome::Sent { .. }
    ));
    let rows = logs(&state, owner, "a2a_response", 2).await;
    assert_eq!(
        rows[0].request["result"]["message"]["parts"][0]["text"],
        "echo: hello agent"
    );
    let stream = rows[1].request["stream_events"].as_array().unwrap();
    assert!(stream.first().unwrap()["task"].is_object());
    assert_eq!(rows[1].request["state"], "completed");
    let task_id = rows[1].request["task_id"].as_str().unwrap().to_owned();
    send(json!({"type":"a2a_get_task","task_id":task_id})).await;
    send(json!({"type":"a2a_send_message","text":"slow job"})).await;
    let rows = logs(&state, owner, "a2a_response", 4).await;
    assert_eq!(
        rows[2].request["result"]["artifacts"][0]["parts"][0]["text"],
        "echo: please make a task"
    );
    assert_eq!(rows[3].request["state"], "working");
    let slow = rows[3].request["task_id"].as_str().unwrap().to_owned();
    send(json!({"type":"a2a_cancel_task","task_id":slow})).await;
    send(json!({"type":"a2a_get_task","task_id":"missing-task"})).await;
    let rows = logs(&state, owner, "a2a_response", 6).await;
    assert_eq!(rows[4].request["state"], "canceled");
    assert_eq!(rows[5].request["error"]["code"], -32001);
    state.remove_client(cid).await;
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}
