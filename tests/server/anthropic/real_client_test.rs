//! The official Anthropic SDKs against NetGet's server, failing rather than skipping when absent:
//! anthropic (Python) and @anthropic-ai/sdk (TypeScript). Each runs a plain message, a stream, a
//! tool call answered with a tool_result, a streamed tool call, count_tokens, the models list
//! and lookup, and four refusals as its own error classes. Peers from `install_peers.py`.
use super::wire_test::{handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

pub fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/anthropic/install_peers.py <root> and export what it prints")
    })
}

async fn run(program: PathBuf, args: Vec<String>) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(&program)
            .args(&args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap_or_else(|_| panic!("{} deadline", program.display()))
    .unwrap_or_else(|e| panic!("{}: {e}", program.display()));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{} failed\nstdout:\n{stdout}\nstderr:\n{}",
        program.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

async fn servers() -> (String, String) {
    let (_s, _i, open) = start(handlers(), None).await;
    let (_s2, _i2, locked) = start(handlers(), Some(json!({"api_key":"sk-ant-netget-test"}))).await;
    // Both servers live as long as the test process; their state is leaked deliberately.
    std::mem::forget((_s, _s2));
    (format!("http://{open}"), format!("http://{locked}"))
}

fn check(got: &Value) {
    let plain = &got["plain"];
    assert!(plain["id"].as_str().unwrap().starts_with("msg_"), "{got}");
    assert_eq!(
        plain["text"], "Echo: hello there | system: Be brief.",
        "{got}"
    );
    assert_eq!(
        (&plain["stop_reason"], &plain["role"], &plain["model"]),
        (
            &json!("end_turn"),
            &json!("assistant"),
            &json!("claude-netget-1")
        )
    );
    assert!(
        plain["usage"][0].as_u64().unwrap() > 0 && plain["usage"][1].as_u64().unwrap() > 0,
        "{got}"
    );
    let stream = &got["stream"];
    let echoed = "Echo: stream this please, with ünïcode ✓";
    assert_eq!(
        (&stream["joined"], &stream["text"]),
        (&json!(echoed), &json!(echoed)),
        "{got}"
    );
    assert!(stream["deltas"].as_u64().unwrap() > 1, "{got}");
    assert_eq!(stream["stop_reason"], "end_turn");
    assert_eq!(
        got["tools"],
        json!({"stop_reason":"tool_use","name":"get_weather","input":{"city":"Paris","days":[1,2]},
               "id_prefix":"toolu_","answer":"Tool said: sunny, 21C"})
    );
    assert_eq!(
        got["stream_tools"],
        json!({"name":"get_weather","input":{"city":"Paris","days":[1,2]},"stop_reason":"tool_use"})
    );
    assert!(got["count"].as_u64().unwrap() > 0, "{got}");
    assert_eq!(got["models"], json!(["claude-netget-1"]));
    assert_eq!(got["model"], "claude-netget-1");
    assert_eq!(
        got["rate_limited"],
        json!({"status":429,"type":"rate_limit_error","message":"slow down"})
    );
    assert_eq!(
        got["invalid"],
        json!({"status":400,"type":"invalid_request_error"})
    );
    assert_eq!(
        got["not_found"],
        json!({"status":404,"type":"not_found_error"})
    );
    assert_eq!(
        got["auth"],
        json!({"status":401,"type":"authentication_error"})
    );
}

#[tokio::test]
async fn anthropic_python_sdk() {
    let (open, locked) = servers().await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/anthropic/sdk_peer.py"
    );
    check(
        &run(
            env_path("NETGET_ANTHROPIC_PYTHON"),
            vec![script.into(), open, locked],
        )
        .await,
    );
}

#[tokio::test]
async fn anthropic_typescript_sdk() {
    let (open, locked) = servers().await;
    let peer = env_path("NETGET_ANTHROPIC_TS_PEER");
    check(
        &run(
            "node".into(),
            vec![peer.display().to_string(), open, locked],
        )
        .await,
    );
}
