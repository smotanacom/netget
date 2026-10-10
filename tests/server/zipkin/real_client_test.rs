//! Independent Zipkin reporters against NetGet's collector, failing rather than skipping when
//! absent: openzipkin/zipkin-go (its tracer, HTTP reporter and model decoder, through
//! `tests/server/zipkin/peer`) and OpenTelemetry Python's Zipkin JSON exporter
//! (`otel_peer.py`). `tests/server/zipkin/install_peers.py` prints NETGET_ZIPKIN_GO_PEER and
//! NETGET_ZIPKIN_PYTHON.
use super::wire_test::{handler_saw, handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/zipkin/install_peers.py <root> and export what it prints")
    })
}

async fn run(program: PathBuf, args: &[&str]) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(&program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer deadline")
    .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)))
}

#[tokio::test]
async fn zipkin_go_reporter_and_decoder() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id, addr) = start(handlers(&dir.path().join("spans.json"))).await;
    let out = run(
        env_path("NETGET_ZIPKIN_GO_PEER"),
        &[&format!("http://{addr}")],
    )
    .await;
    // The 429 the handler gave refuse-me reached zipkin-go's reporter, and nothing else failed.
    assert_eq!(
        out["reporter_log"], "failed the request with status code 429",
        "{out}"
    );
    assert_eq!(out["services_status"], 200);
    assert_eq!(out["services"], json!(["go-frontend"]));
    assert_eq!(out["trace_status"], 200, "{out}");
    assert_eq!(out["missing_status"], 404);
    // zipkin-go's own decoder read back the trace it reported, ids and structure intact.
    let trace = out["trace"].as_array().unwrap();
    assert_eq!(trace.len(), 2, "{out}");
    let root = trace.iter().find(|s| s["name"] == "checkout").unwrap();
    let child = trace.iter().find(|s| s["name"] == "charge-card").unwrap();
    assert_eq!(root["kind"], "SERVER");
    assert_eq!(root["tags"], json!({"http.method": "POST"}));
    assert_eq!(root["annotations"], json!(["cart-loaded"]));
    assert_eq!(child["kind"], "CLIENT");
    assert_eq!(child["parent"], out["root_id"]);
    assert_eq!(child["remote"], "payments");
    assert_eq!(child["service"], "go-frontend");
    assert!(child["duration_us"].as_i64().unwrap() >= 2000, "{out}");
    assert!(trace.iter().all(|s| s["trace_id"] == out["trace_id"]));
    assert!(handler_saw(&state, id, "\"span_count\":2").await);
}

#[tokio::test]
async fn opentelemetry_python_exporter() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("spans.json");
    let (state, id, addr) = start(handlers(&store)).await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/zipkin/otel_peer.py"
    );
    let out = run(
        env_path("NETGET_ZIPKIN_PYTHON"),
        &["-I", script, &format!("http://{addr}")],
    )
    .await;
    // The exporter reports FAILURE for any non-2xx answer, so the 429 is visible to it.
    assert_eq!(out["accepted"], "SUCCESS", "{out}");
    assert_eq!(out["refused"], "FAILURE", "{out}");
    assert!(handler_saw(&state, id, "otel-checkout").await);
    // What the exporter sent validated as v2 spans: 128-bit trace id, parent link, kinds,
    // the attribute as a tag and the event as an annotation.
    let stored: Vec<Value> = serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|s| s["traceId"] == out["trace_id"]));
    let root = stored.iter().find(|s| s["name"] == "place-order").unwrap();
    let child = stored
        .iter()
        .find(|s| s["name"] == "reserve-stock")
        .unwrap();
    assert_eq!(root["kind"], "SERVER");
    assert_eq!(root["localEndpoint"]["serviceName"], "otel-checkout");
    assert_eq!(root["tags"]["order.id"], "42");
    // OpenTelemetry writes an event as a JSON object keyed by its name.
    assert_eq!(root["annotations"][0]["value"], r#"{"validated": {}}"#);
    assert_eq!(child["kind"], "CLIENT");
    assert_eq!(child["parentId"], root["id"]);
}
