//! NetGet's Zipkin reporter against two independent servers, failing rather than skipping
//! when absent (`install_peers.py` fetches both by SHA-256): the official Zipkin server
//! (zipkin-server 3.5.1, in-memory storage) read back through its own /api/v2 read API, and
//! Jaeger 1.62's Zipkin collector read back through Jaeger's own query API. In both, the
//! spans on the server are the ones the connect handler's zipkin_report put on the wire.
use super::session_test::{client, send, wait_log};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::{json, Value};
use std::time::Duration;

const TRACE: &str = "463ac35c9f6413ad48485a3953bb6124";

fn env(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| {
        panic!("{var} is required: run tests/client/zipkin/install_peers.py <root> and export what it prints")
    })
}

#[tokio::test]
async fn netget_against_official_zipkin_server() {
    let server = RealServer::builder(
        &env("NETGET_ZIPKIN_JAVA"),
        InstallHint {
            brew: "openjdk (and tests/client/zipkin/install_peers.py)",
            apt: "openjdk-21-jre-headless (and tests/client/zipkin/install_peers.py)",
        },
    )
    .args([
        "-jar".to_string(),
        env("NETGET_ZIPKIN_JAR"),
        "--server.port={port}".into(),
        "--armeria.ports[0].port={port}".into(),
        "--armeria.ports[0].ip=127.0.0.1".into(),
        "--armeria.ports[0].protocols[0]=http".into(),
    ])
    .ready_when_log_matches("Serving HTTP at")
    .startup_timeout(Duration::from_secs(120))
    .start()
    .await
    .expect("start zipkin-server");
    let (state, id) = client(server.addr(), json!([])).await;
    let report = wait_log(&state, id, "zipkin_report_result").await;
    assert!(report.contains(r#""status":202"#), "{report}");
    let services = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"services"}),
    )
    .await;
    assert_eq!(services["result"], json!(["netget-shop"]), "{services}");
    let names = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"spans","query":{"serviceName":"netget-shop"}}),
    )
    .await;
    assert_eq!(names["result"], json!(["charge", "checkout"]), "{names}");
    let remote = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"remoteServices","query":{"serviceName":"netget-shop"}}),
    )
    .await;
    assert_eq!(remote["result"], json!(["payments"]), "{remote}");
    // Zipkin's own decode of what NetGet sent, read back through NetGet's decoder.
    let trace = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"trace","trace_id":TRACE}),
    )
    .await;
    let spans = trace["result"].as_array().expect("a trace");
    assert_eq!(spans.len(), 2, "{trace}");
    let checkout = spans.iter().find(|s| s["name"] == "checkout").unwrap();
    let charge = spans.iter().find(|s| s["name"] == "charge").unwrap();
    assert_eq!(checkout["tags"]["cart.items"], "3");
    assert_eq!(checkout["duration"], 5000);
    assert_eq!(charge["parentId"], "a2fb4a1d1a96d312");
    assert_eq!(charge["id"], "0020000000000001");
    assert_eq!(charge["annotations"][0]["value"], "card-sent");
    let traces = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"traces","query":{"serviceName":"netget-shop","endTs":1700000001000u64,"lookback":86400000}}),
    )
    .await;
    assert_eq!(
        traces["result"][0].as_array().map(Vec::len),
        Some(2),
        "{traces}"
    );
    let missing = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"trace","trace_id":"00000000000000ff"}),
    )
    .await;
    assert_eq!(missing["status"], 404, "{missing}");
    assert!(
        missing["message"].as_str().unwrap().contains("not found"),
        "{missing}"
    );
    state.remove_client(id).await;
}

#[tokio::test]
async fn netget_against_jaeger_zipkin_collector() {
    // Every other listener Jaeger opens goes to an OS-assigned loopback port.
    let loopback = "127.0.0.1:0";
    let mut args = vec![
        "--collector.zipkin.host-port=127.0.0.1:{port}".to_string(),
        "--query.http-server.host-port=127.0.0.1:{port1}".to_string(),
    ];
    for flag in [
        "admin.http.host-port",
        "collector.grpc-server.host-port",
        "collector.http-server.host-port",
        "collector.otlp.grpc.host-port",
        "collector.otlp.http.host-port",
        "http-server.host-port",
        "processor.jaeger-binary.server-host-port",
        "processor.jaeger-compact.server-host-port",
        "processor.zipkin-compact.server-host-port",
        "query.grpc-server.host-port",
    ] {
        args.push(format!("--{flag}={loopback}"));
    }
    let server = RealServer::builder(
        &env("NETGET_JAEGER_BIN"),
        InstallHint {
            brew: "jaeger (or tests/client/zipkin/install_peers.py)",
            apt: "tests/client/zipkin/install_peers.py",
        },
    )
    .args(args)
    .extra_ports(1)
    .ready_when_log_matches("Query server started")
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
    .expect("start jaeger-all-in-one");
    let query = format!("http://127.0.0.1:{}", server.extra_ports[0]);
    let (state, id) = client(server.addr(), json!([])).await;
    let report = wait_log(&state, id, "zipkin_report_result").await;
    assert!(report.contains(r#""status":202"#), "{report}");
    // Jaeger has no Zipkin read API; its own query API shows what its collector decoded.
    let http = reqwest::Client::new();
    let trace: Value = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let r: Value = http
                .get(format!("{query}/api/traces/{TRACE}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if r["data"][0]["spans"]
                .as_array()
                .is_some_and(|s| s.len() == 2)
            {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("jaeger stored the trace");
    let t = &trace["data"][0];
    let spans = t["spans"].as_array().unwrap();
    let charge = spans
        .iter()
        .find(|s| s["operationName"] == "charge")
        .unwrap();
    let checkout = spans
        .iter()
        .find(|s| s["operationName"] == "checkout")
        .unwrap();
    assert_eq!(charge["references"][0]["refType"], "CHILD_OF");
    assert_eq!(charge["references"][0]["spanID"], "a2fb4a1d1a96d312");
    assert_eq!(checkout["duration"], 5000);
    assert_eq!(charge["startTime"], 1700000000001000u64);
    let tag = |s: &Value, k: &str| {
        s["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["key"] == k)
            .map(|t| t["value"].clone())
    };
    assert_eq!(tag(checkout, "cart.items"), Some(json!("3")));
    assert_eq!(tag(charge, "span.kind"), Some(json!("client")));
    assert_eq!(charge["logs"][0]["fields"][0]["value"], "card-sent");
    let process = &t["processes"][checkout["processID"].as_str().unwrap()];
    assert_eq!(process["serviceName"], "netget-shop");
    let services: Value = http
        .get(format!("{query}/api/services"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        services["data"]
            .as_array()
            .unwrap()
            .contains(&json!("netget-shop")),
        "{services}"
    );
    state.remove_client(id).await;
}
