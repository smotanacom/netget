use super::common::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
async fn head(peer: &mut TcpStream) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut data = Vec::new();
        while !data.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            peer.read_exact(&mut byte).await.unwrap();
            data.push(byte[0]);
            assert!(data.len() < 8192);
        }
        String::from_utf8(data).unwrap()
    })
    .await
    .unwrap()
}
async fn response(peer: &mut TcpStream, status: u16, headers: &str, body: &[u8]) {
    peer.write_all(format!("HTTP/1.1 {status} Response\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",body.len()).as_bytes()).await.unwrap();
    peer.write_all(body).await.unwrap();
}
fn metric(data: &Value, name: &str) -> Value {
    data["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap()
        .clone()
}
#[tokio::test]
async fn netget_exporter_pair_negotiates_all_classic_types_and_timestamp_units() {
    let state = state();
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let metrics = json!([
        {"name":"requests","type":"counter","help":"Requests\\nby path","samples":[{"labels":{"path":"C:\\logs \"one\"\nnext"},"value":3}]},
        {"name":"in_flight","type":"gauge","help":"Current requests","samples":[{"value":4,"timestamp_ms":1605281325125_i64}]},
        {"name":"latency_seconds","type":"histogram","samples":[{"suffix":"_bucket","labels":{"le":"0.5"},"value":2},{"suffix":"_bucket","labels":{"le":"+Inf"},"value":3},{"suffix":"_count","value":3},{"suffix":"_sum","value":1.5}]},
        {"name":"rpc_seconds","type":"summary","samples":[{"labels":{"quantile":"0.99"},"value":0.25},{"suffix":"_sum","value":2},{"suffix":"_count","value":5}]},
        {"name":"special","type":"untyped","samples":[{"labels":{"kind":"nan"},"value":"NaN"},{"labels":{"kind":"inf"},"value":"+Inf"}]}
    ]);
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let sid = netget::cli::management::ServerForm {
        protocol: "prometheus".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        event_handlers: Some(vec![static_handler(
            "prometheus_scrape",
            json!([{"type":"send_metrics","metrics":metrics}]),
        )]),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(addr) = state.get_server(sid).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let id = connected_client(&state, addr.to_string()).await;
    for format in ["text", "openmetrics", "auto"] {
        let result = request(&state, id, json!({"format":format})).await;
        assert_eq!(result["sample_count"], 11);
        assert_eq!(
            result["format"],
            if format == "text" {
                "text"
            } else {
                "openmetrics"
            }
        );
        let counter = metric(
            &result,
            if format == "text" {
                "requests_total"
            } else {
                "requests"
            },
        );
        assert_eq!(counter["samples"][0]["name"], "requests_total");
        assert_eq!(counter["samples"][0]["value"], 3.0);
        assert_eq!(
            counter["samples"][0]["labels"]["path"],
            "C:\\logs \"one\"\nnext"
        );
        let gauge = metric(&result, "in_flight");
        if format == "text" {
            assert_eq!(gauge["samples"][0]["timestamp_ms"], 1605281325125_i64);
        } else {
            assert_eq!(gauge["samples"][0]["timestamp_seconds"], 1605281325.125);
        }
        assert_eq!(
            metric(&result, "latency_seconds")["samples"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            metric(&result, "rpc_seconds")["samples"][0]["labels"]["quantile"],
            "0.99"
        );
        assert_eq!(metric(&result, "special")["samples"][0]["value"], "NaN");
    }
    state.remove_client(id).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn fragmented_body_survives_busy_injection_and_request_headers_are_typed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        let request = head(&mut peer).await.to_ascii_lowercase();
        assert!(request.starts_with("get /custom?target=local http/1.1"));
        assert!(request.contains("escaping=underscores"));
        assert!(request.contains("accept-encoding: identity"));
        assert!(request.contains("x-prometheus-scrape-timeout-seconds: 3"));
        peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain;version=0.0.4\r\nContent-Length: 8\r\nConnection: close\r\n\r\nx ").await.unwrap();
        started_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        for byte in b"1\ny 2\n" {
            peer.write_all(&[*byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!({"metrics_path":"/custom?target=local","scrape_timeout_secs":3}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    event(&state, id, "prometheus_connected", 0).await;
    send(&state, id, json!({"format":"auto"})).await;
    started_rx.await.unwrap();
    rejected(
        &state,
        id,
        json!({"type":"scrape_metrics"}),
        "scrape pending",
    )
    .await;
    resume_tx.send(()).unwrap();
    let (_, result) = event(&state, id, "prometheus_metrics", 0).await;
    assert_eq!(result["sample_count"], 2);
    assert_eq!(result["path"], "/custom?target=local");
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn failed_scrapes_emit_no_partial_metrics_and_allow_recovery_without_redirects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        for (status, headers, body) in [
            (302, "Location: /metrics\r\n", b"redirect".as_slice()),
            (
                503,
                "Content-Type: text/plain\r\n",
                b"unavailable".as_slice(),
            ),
            (200, "Content-Type: application/json\r\n", b"{}".as_slice()),
            (
                200,
                "Content-Type: text/plain\r\nContent-Encoding: gzip\r\n",
                b"x 1\n".as_slice(),
            ),
            (
                200,
                "Content-Type: application/openmetrics-text;version=1.0.0\r\n",
                b"x 1\n".as_slice(),
            ),
            (
                200,
                "Content-Type: text/plain\r\n",
                b"x 1\nx 2\n".as_slice(),
            ),
            (200, "Content-Type: text/plain\r\n", b"x 1\n".as_slice()),
        ] {
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            response(&mut peer, status, headers, body).await;
        }
    });
    let state = state();
    let id = connected_client(&state, addr.to_string()).await;
    for needle in [
        "HTTP 302",
        "HTTP 503",
        "Content-Type",
        "Content-Encoding",
        "missing EOF",
        "duplicate metric series",
    ] {
        let after = latest(&state, id).await;
        send(&state, id, json!({})).await;
        let (_, error) = event(&state, id, "prometheus_scrape_error", after).await;
        assert!(error["error"].as_str().unwrap().contains(needle), "{error}");
        assert!(error.get("metrics").is_none());
        assert!(matches!(
            state.get_client(id).await.unwrap().status,
            netget::state::ClientStatus::Connected
        ));
    }
    assert_eq!(request(&state, id, json!({})).await["sample_count"], 1);
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn disconnect_and_removal_cancel_stalled_scrapes_with_manual_connected_handler() {
    for remove in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = oneshot::channel();
        let fixture = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            ready_tx.send(()).unwrap();
            let mut data = Vec::new();
            peer.read_to_end(&mut data).await.unwrap();
        });
        let state = state();
        let id = client(
            &state,
            addr.to_string(),
            json!({}),
            vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":60}})],
        )
        .await;
        assert_eq!(state.client_task_count(id).await, 2);
        send(&state, id, json!({})).await;
        ready_rx.await.unwrap();
        if remove {
            state.remove_client(id).await;
        } else {
            let outcome = state
                .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
                .await
                .unwrap();
            assert!(matches!(
                outcome,
                netget::state::client_handles::ClientSendOutcome::Disconnected
            ));
        }
        tokio::time::timeout(Duration::from_secs(1), fixture)
            .await
            .unwrap()
            .unwrap();
        state.remove_client(id).await;
        assert_eq!(state.client_task_count(id).await, 0);
    }
}
#[tokio::test]
async fn whole_scrape_deadline_closes_partial_body_and_reports_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        head(&mut peer).await;
        peer.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\nx 1\n",
        )
        .await
        .unwrap();
        let mut data = Vec::new();
        peer.read_to_end(&mut data).await.unwrap();
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!({"scrape_timeout_secs":1}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    send(&state, id, json!({})).await;
    let (_, result) = event(&state, id, "prometheus_scrape_error", 0).await;
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("deadline") || error.contains("timed out"),
        "{error}"
    );
    tokio::time::timeout(Duration::from_secs(2), fixture)
        .await
        .unwrap()
        .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn content_length_and_chunked_body_limits_are_enforced() {
    for chunked in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            if chunked {
                peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                let mut remaining = netget::client::prometheus::exposition::MAX_BODY + 1;
                while remaining > 0 {
                    let n = remaining.min(16384);
                    if peer
                        .write_all(format!("{n:x}\r\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if peer.write_all(&vec![b' '; n]).await.is_err() {
                        break;
                    }
                    if peer.write_all(b"\r\n").await.is_err() {
                        break;
                    }
                    remaining -= n;
                }
                let _ = peer.write_all(b"0\r\n\r\n").await;
            } else {
                peer.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                        netget::client::prometheus::exposition::MAX_BODY + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            }
        });
        let state = state();
        let id = connected_client(&state, addr.to_string()).await;
        send(&state, id, json!({})).await;
        let (_, error) = event(&state, id, "prometheus_scrape_error", 0).await;
        assert!(
            error["error"].as_str().unwrap().contains("body limit"),
            "{error}"
        );
        fixture.await.unwrap();
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn parked_dispatcher_has_a_bounded_event_queue() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        for _ in 0..=netget::client::prometheus::QUEUE_CAPACITY {
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            response(&mut peer, 200, "Content-Type: text/plain\r\n", b"x 1\n").await;
        }
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":60}})],
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5),async {while !state.list_intercepts().await.iter().any(|e|matches!(e.owner,netget::state::intercepts::InterceptOwner::Client(owner) if owner==id)) {tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
    for _ in 0..=netget::client::prometheus::QUEUE_CAPACITY {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let outcome = state
                    .send_to_client(id, json!({"type":"scrape_metrics"}), Duration::from_secs(1))
                    .await
                    .unwrap();
                match outcome {
                    netget::state::client_handles::ClientSendOutcome::Executed { .. } => break,
                    netget::state::client_handles::ClientSendOutcome::Rejected { error } => {
                        assert!(error.contains("scrape pending"), "{error}")
                    }
                    other => panic!("{other:?}"),
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    failed(&state, id, "event queue full").await;
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn handler_followups_stop_at_four_then_injection_starts_a_new_chain() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        for n in 0..=netget::client::prometheus::MAX_FOLLOWUPS {
            let (mut peer, _) = listener.accept().await.unwrap();
            let request = head(&mut peer).await;
            assert!(
                request.starts_with(if n == netget::client::prometheus::MAX_FOLLOWUPS {
                    "GET /injected "
                } else {
                    "GET /metrics "
                }),
                "{request}"
            );
            response(&mut peer, 200, "Content-Type: text/plain\r\n", b"x 1\n").await;
        }
    });
    let state = state();
    let id=client(&state,addr.to_string(),json!({}),vec![
        static_handler("prometheus_connected",json!([{"type":"scrape_metrics"}])),
        json!({"event_pattern":"prometheus_metrics","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\njson.dump({'actions':[{'type':'scrape_metrics'}] if e['path']=='/metrics' else []},sys.stdout)"}}),
    ]).await;
    let mut after = 0;
    for _ in 0..netget::client::prometheus::MAX_FOLLOWUPS {
        after = event(&state, id, "prometheus_metrics", after).await.0;
    }
    send(&state, id, json!({"path":"/injected"})).await;
    assert_eq!(
        event(&state, id, "prometheus_metrics", after).await.1["path"],
        "/injected"
    );
    fixture.await.unwrap();
    state.remove_client(id).await;
}
