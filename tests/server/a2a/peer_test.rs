//! a2a-sdk 1.2.1's client — an independent A2A 1.0 implementation, unchanged — against
//! NetGet's agent: card resolution, a direct message, a streamed task (snapshot, working,
//! artifact, completed), GetTask, a working task and its cancellation. Fails, never skips.
use crate::helpers::a2a::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn a2a_sdk_client_runs_messages_streams_and_tasks_against_netget() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        echo_agent_policy(),
        json!({"agent_name":"NetGet Echo"}),
    )
    .await;
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(python())
            .arg(peer_script())
            .args(["client", &format!("http://{addr}/")])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("a2a-sdk client timed out")
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let steps: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let of = |n: &str| {
        steps
            .iter()
            .filter(|s| s["step"] == n)
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(of("card")[0]["name"], "NetGet Echo");
    assert_eq!(
        of("message")[0]["event"]["message"]["parts"][0]["text"],
        "echo: hello agent"
    );
    assert_eq!(of("message")[0]["event"]["message"]["role"], "ROLE_AGENT");
    let stream = of("stream");
    assert_eq!(
        stream.first().unwrap()["event"]["task"]["status"]["state"],
        "TASK_STATE_SUBMITTED"
    );
    assert!(stream.iter().any(
        |s| s["event"]["artifactUpdate"]["artifact"]["parts"][0]["text"]
            == "echo: please make a task"
    ));
    assert_eq!(
        stream.last().unwrap()["event"]["statusUpdate"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        of("get")[0]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        of("slow")[0]["event"]["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    assert_eq!(
        of("cancel")[0]["task"]["status"]["state"],
        "TASK_STATE_CANCELED"
    );
    let server = AccessLogOwner::Server(sid.as_u32());
    let msgs = logs(&state, server, "a2a_message", 3).await;
    assert_eq!(msgs[1].request["method"], "SendStreamingMessage");
    let tasks = logs(&state, server, "a2a_task_request", 2).await;
    assert_eq!(tasks[1].request["method"], "CancelTask");
    state.remove_server(sid).await;
}
