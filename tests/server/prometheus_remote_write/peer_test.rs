use crate::helpers::prometheus_remote_write::{exporter, prometheus_service, server};
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn official_prometheus_sender_scrapes_official_exporter_and_native_collector_observes_typed_v1(
) {
    let exporter = exporter().await;
    let (state, id, addr, _) = server(
        None,
        Some(json!({"auth_token":"peer-secret","path":"/receive"})),
    )
    .await;
    let config=format!("global:\n  scrape_interval: 1s\nscrape_configs:\n  - job_name: peer\n    static_configs:\n      - targets: ['{}']\nremote_write:\n  - url: http://{addr}/receive\n    protobuf_message: prometheus.WriteRequest\n    authorization:\n      type: Bearer\n      credentials: peer-secret\n    send_exemplars: false\n    send_native_histograms: false\n    metadata_config:\n      send: false\n    write_relabel_configs:\n      - source_labels: [__name__]\n        regex: peer_remote_.*\n        action: keep\n    queue_config:\n      capacity: 1000\n      max_shards: 1\n      min_shards: 1\n      max_samples_per_send: 100\n      batch_send_deadline: 100ms\n",exporter.addr());
    let service = prometheus_service(&config).await;
    let series = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let rows = state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await;
            let series = rows
                .into_iter()
                .filter(|r| r.event_type == "remote_write_request")
                .flat_map(|r| r.request["series"].as_array().unwrap().clone())
                .collect::<Vec<_>>();
            if series
                .iter()
                .any(|s| s["labels"]["__name__"] == "peer_remote_value")
                && series
                    .iter()
                    .any(|s| s["labels"]["__name__"] == "peer_remote_nan")
            {
                break series;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "required actual sender produced no samples: {}",
            service.log()
        )
    });
    let finite = series
        .iter()
        .find(|s| s["labels"]["__name__"] == "peer_remote_value")
        .unwrap();
    assert_eq!(finite["labels"]["site"], "edge 名");
    assert_eq!(finite["labels"]["job"], "peer");
    assert_eq!(finite["labels"]["instance"], exporter.addr());
    assert_eq!(finite["samples"][0]["value"], 21.5);
    assert!(finite["samples"][0]["timestamp_ms"].as_i64().unwrap() > 1700000000000);
    let nan = series
        .iter()
        .find(|s| s["labels"]["__name__"] == "peer_remote_nan")
        .unwrap();
    assert_eq!(nan["samples"][0]["value"], "nan");
    let rows = state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await;
    assert!(!serde_json::to_string(&rows)
        .unwrap()
        .contains("peer-secret"));
    state.remove_server(id).await;
}
#[tokio::test]
async fn official_sender_retries_503_without_losing_the_exact_initial_batch() {
    let exporter = exporter().await;
    let code="import json,sys\ni=json.load(sys.stdin)\nn=int(i['server'].get('memory') or '0')\na={'type':'reject_remote_write_samples','status':503,'message':'retry fixture'} if n==0 else {'type':'accept_remote_write_samples'}\nprint(json.dumps({'actions':[{'type':'set_memory','value':str(n+1)},a]}))";
    let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"remote_write_request","handler":{"type":"script","language":"python","code":code}})]),None).await;
    let config=format!("global:\n  scrape_interval: 1s\nscrape_configs:\n  - job_name: peer\n    static_configs:\n      - targets: ['{}']\nremote_write:\n  - url: http://{addr}/api/v1/write\n    protobuf_message: prometheus.WriteRequest\n    metadata_config:\n      send: false\n    write_relabel_configs:\n      - source_labels: [__name__]\n        regex: peer_remote_.*\n        action: keep\n    queue_config:\n      capacity: 1000\n      max_shards: 1\n      min_shards: 1\n      batch_send_deadline: 100ms\n      min_backoff: 100ms\n      max_backoff: 1s\n",exporter.addr());
    let service = prometheus_service(&config).await;
    let rows = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut rows = state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == "remote_write_request")
                .collect::<Vec<_>>();
            if rows.len() >= 2 {
                rows.sort_by_key(|r| r.id);
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "required official sender did not retry503: {}",
            service.log()
        )
    });
    assert_eq!(rows[0].request["series"], rows[1].request["series"]);
    assert!(rows[0]
        .response
        .iter()
        .any(|a| a["type"] == "reject_remote_write_samples"));
    assert!(rows[1]
        .response
        .iter()
        .any(|a| a["type"] == "accept_remote_write_samples"));
    assert!(state.get_memory(id).await.unwrap().parse::<u32>().unwrap() >= 2);
    state.remove_server(id).await;
}
