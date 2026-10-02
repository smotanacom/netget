use super::e2e_test::{batch, send, start};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
#[tokio::test]
async fn official_fluentd_in_forward_observes_four_modes_and_sends_matching_acks() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/client/fluent_forward/peer_receiver.rb");
    let ruby = std::env::var("NETGET_FORWARD_RUBY")
        .expect("NETGET_FORWARD_RUBY missing; bootstrap pinned Fluentd peers");
    let mut child = tokio::process::Command::new(ruby)
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let marker = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("official Fluentd peer exited before bind");
    let addr = marker
        .strip_prefix("NETGET_ADDR ")
        .expect("official input listening marker");
    let (state, id) = start(addr.into(), json!({"type":"static","actions":[]})).await;
    for (index, mode) in ["message", "forward", "packed", "compressed_packed"]
        .iter()
        .enumerate()
    {
        let mut b = batch(mode, true);
        b["entries"][0]["record"]["mode"] = json!(mode);
        send(&state, id, b).await;
        let marker = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("Fluentd typed observation");
        let observed: Value = serde_json::from_str(
            marker
                .strip_prefix("NETGET_RECORD ")
                .expect("Fluentd record marker"),
        )
        .unwrap();
        assert_eq!(observed["tag"], "demo.logs");
        assert_eq!(observed["record"]["message"], "温度");
        assert_eq!(observed["record"]["mode"], *mode);
        assert_eq!(observed["timestamp"]["seconds"], 1700000000);
        assert_eq!(observed["timestamp"]["nanoseconds"], 250000000);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let e = state
                    .list_access_logs_for(
                        Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                        None,
                    )
                    .await;
                if e.iter().filter(|e| e.event_type == "forward_ack").count() == index + 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
