use crate::helpers::prometheus_remote_write::{client, logs, prometheus_service, query};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[tokio::test]
async fn official_prometheus_receives_float_order_nan_inf_and_exact_stale_semantics() {
    let service = prometheus_service("global:\n  scrape_interval: 1s\nscrape_configs: []\n").await;
    let (state, id) = client(service.addr(), None, None).await;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let mut series = vec![
        json!({"labels":{"__name__":"peer_remote_value","site":"edge 名"},"samples":[{"timestamp_ms":now-4000,"value":1.5},{"timestamp_ms":now-3000,"value":2.5}]}),
        json!({"labels":{"__name__":"peer_remote_stale"},"samples":[{"timestamp_ms":now-5000,"value":7},{"timestamp_ms":now-1000,"value":"stale"}]}),
    ];
    for (metric, value) in [
        ("peer_remote_nan", "nan"),
        ("peer_remote_pinf", "+inf"),
        ("peer_remote_ninf", "-inf"),
    ] {
        series.push(json!({"labels":{"__name__":metric},"samples":[{"timestamp_ms":now-2000,"value":value}]}));
    }
    let out = state
        .send_to_client(
            id,
            json!({"type":"write_remote_samples","batch":{"series":series}}),
            Duration::from_secs(15),
        )
        .await
        .unwrap();
    assert!(matches!(out, ClientSendOutcome::Executed { .. }));
    let rows = logs(
        &state,
        AccessLogOwner::Client(id.as_u32()),
        "remote_write_response",
        1,
    )
    .await;
    assert_eq!(rows[0].request["status"], 204);
    assert_eq!(rows[0].request["accepted"], true);
    for (metric, expected) in [
        ("peer_remote_value", "2.5"),
        ("peer_remote_nan", "NaN"),
        ("peer_remote_pinf", "+Inf"),
        ("peer_remote_ninf", "-Inf"),
    ] {
        let result = query(&service, metric, None).await;
        assert_eq!(result["status"], "success");
        assert_eq!(
            result["data"]["result"][0]["value"][1], expected,
            "{result}"
        );
    }
    let result = query(&service, "peer_remote_stale", None).await;
    assert_eq!(result["data"]["result"], json!([]));
    let result = query(&service, "peer_remote_stale", Some(now - 3000)).await;
    assert_eq!(result["data"]["result"][0]["value"][1], "7");
    let out=state.send_to_client(id,json!({"type":"write_remote_samples","batch":{"series":[{"labels":{"__name__":"peer_remote_value","site":"edge 名"},"samples":[{"timestamp_ms":now-3000,"value":9}]}]}}),Duration::from_secs(15)).await.unwrap();
    assert!(matches!(out, ClientSendOutcome::Executed { .. }));
    let rows = logs(
        &state,
        AccessLogOwner::Client(id.as_u32()),
        "remote_write_response",
        2,
    )
    .await;
    assert_eq!(rows[1].request["status"], 400);
    assert_eq!(rows[1].request["accepted"], false);
    assert_eq!(rows[1].request["attempts"], 1);
    state.remove_client(id).await;
}
