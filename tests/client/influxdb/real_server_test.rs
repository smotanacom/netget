use super::e2e_test::{batch, response_logs, send, start};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
#[tokio::test]
async fn official_decoder_backed_receiver_validates_http_and_observes_five_types_four_precisions_gzip(
) {
    let receiver = std::env::var("NETGET_INFLUX_RECEIVER").expect(
        "NETGET_INFLUX_RECEIVER missing; bootstrap pinned independent decoder-backed receiver",
    );
    let mut child = tokio::process::Command::new(receiver)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let marker = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("decoder receiver bind");
    let addr = marker.strip_prefix("NETGET_ADDR ").unwrap();
    let (state, id) = start(addr.into(), None, Some("peer-secret")).await;
    for (index, (precision, gzip)) in ["ns", "us", "ms", "s"]
        .into_iter()
        .flat_map(|p| [(p, false), (p, true)])
        .enumerate()
    {
        assert!(matches!(
            send(&state, id, batch(precision, gzip)).await,
            netget::state::client_handles::ClientSendOutcome::Executed { .. }
        ));
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("official decoder observation");
        let e: Value = serde_json::from_str(line.strip_prefix("NETGET_RECORD ").unwrap()).unwrap();
        assert_eq!(e["org"], "org 名 &");
        assert_eq!(e["bucket"], "bucket / &");
        assert_eq!(e["encoding"], if gzip { "gzip" } else { "identity" });
        assert_eq!(e["precision"], precision);
        assert_eq!(e["points"][0]["measurement"], "温 度,");
        assert_eq!(e["points"][0]["tags"]["host =,"], "a,b =名");
        assert_eq!(
            e["points"][0]["fields"],
            batch(precision, gzip)["points"][0]["fields"]
        );
        let factor = match precision {
            "ns" => 1,
            "us" => 1000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            _ => unreachable!(),
        };
        assert_eq!(e["points"][0]["timestamp_ns"], 123i64 * factor);
        assert_eq!(
            response_logs(&state, id, index + 1).await[index].request["status"],
            204
        );
    }
    let mut b = batch("ns", false);
    b["bucket"] = json!("reject");
    send(&state, id, b).await;
    lines.next_line().await.unwrap().unwrap();
    let entries = response_logs(&state, id, 9).await;
    assert_eq!(entries[8].request["status"], 429);
    assert_eq!(entries[8].request["error"]["message"], "rate limited");
    assert_eq!(entries[8].request["retry_after_seconds"], 3);
    assert!(!entries
        .iter()
        .any(|e| e.request.to_string().contains("peer-secret")));
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
