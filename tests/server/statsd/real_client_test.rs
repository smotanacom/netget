//! Requires python3 plus datadog==0.52.0 and statsd==4.0.1. Missing peers FAIL.
use super::e2e_test::{logs, start};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn independent_python_statsd_and_datadog_emitters_reach_typed_collection() {
    let (state, id, addr) = start(None, None).await;
    let source = r#"
import sys, os
for key in list(os.environ):
    if key.startswith('DD_') or key == 'DATADOG_TAGS':
        os.environ.pop(key)
from statsd import StatsClient
from datadog import DogStatsd
port=int(sys.argv[1])
plain=StatsClient('127.0.0.1',port)
with plain.pipeline() as p:
    p.incr('python.counter',3)
    p.gauge('python.gauge',-2,delta=True)
    p.timing('python.timer',12)
    p.set('python.set','alice')
dog=DogStatsd(host='127.0.0.1',port=port,disable_telemetry=True,disable_buffering=False,origin_detection_enabled=False)
with dog:
    dog.increment('dog.counter',2,tags=['env:test'])
    dog.histogram('dog.histogram',3.5)
    dog.distribution('dog.distribution',4.5)
    dog.event('雪','hello\nworld',alert_type='warning',tags=['lang:unicode'])
    dog.service_check('dog.check',2,message='m:bad\nconnection',tags=['env:test'])
dog.close_socket()
"#;
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new("python3")
            .args(["-c", source, &addr.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("reference emitter timeout")
    .expect("python3 required");
    assert!(
        output.status.success(),
        "Install pinned reference packages; no skip permitted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entries = logs(&state, id, 2).await;
    let records: Vec<_> = entries
        .iter()
        .flat_map(|entry| entry.request["records"].as_array().into_iter().flatten())
        .collect();
    assert_eq!(records.len(), 9, "received {entries:?}");
    assert!(records
        .iter()
        .any(|r| r["name"] == "python.counter" && r["value"] == "3"));
    assert!(records
        .iter()
        .any(|r| r["name"] == "python.gauge" && r["value"] == "-2"));
    assert!(records
        .iter()
        .any(|r| r["name"] == "python.timer" && r["metric_type"] == "ms"));
    assert!(records
        .iter()
        .any(|r| r["name"] == "python.set" && r["value"] == "alice"));
    assert!(records
        .iter()
        .any(|r| r["name"] == "dog.counter" && r["tags"] == json!(["env:test"])));
    assert!(records
        .iter()
        .any(|r| r["name"] == "dog.histogram" && r["metric_type"] == "h"));
    assert!(records
        .iter()
        .any(|r| r["name"] == "dog.distribution" && r["metric_type"] == "d"));
    assert!(records.iter().any(|r| r["kind"] == "event"
        && r["title"] == "雪"
        && r["text"] == "hello\nworld"
        && r["alert_type"] == "warning"));
    assert!(records.iter().any(|r| r["kind"] == "service_check"
        && r["status"] == 2
        && r["message"] == "m:bad\nconnection"));
    state.remove_server(id).await;
}
