use super::common::*;
use netget::{
    client::otlp::{actions::OtlpClientProtocol, wire},
    llm::actions::{client_trait::Client, protocol_trait::Protocol},
    state::client_handles::ClientSendOutcome,
};
use serde_json::{json, Value};
use std::time::Duration;
#[tokio::test]
async fn official_collector_all_signals_plain_and_gzip_on_both_transports() {
    for transport in ["grpc", "http"] {
        let mut peer = Collector::start(transport, false).await;
        let state = state();
        let mut params = peer.params();
        params["gzip"] = json!(false);
        let id = client(&state, peer.port, params, vec![empty()])
            .await
            .unwrap();
        for (index, action) in exports().into_iter().enumerate() {
            assert!(matches!(
                send(&state, id, action).await.unwrap(),
                ClientSendOutcome::Executed { .. }
            ));
            let result = event(&state, id, "otlp_export_result", index).await;
            assert_eq!(result["result"], "accepted");
            assert_eq!(result["transport"], transport);
            assert_eq!(result["items"], if index == 1 { 2 } else { 1 });
        }
        let output = peer
            .wait_output(&[
                "collector trace marker",
                "collector.queue.depth",
                "collector log marker",
                "payments",
                "Value: 3.500000",
                "checkout",
            ])
            .await;
        println!("Collector0.162.0 {transport} decoded all signals:\n{output}");
        send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap();
        let gzip_id = client(&state, peer.port, peer.params(), vec![empty()])
            .await
            .unwrap();
        for (index, action) in exports().into_iter().enumerate() {
            send(&state, gzip_id, action).await.unwrap();
            assert_eq!(
                event(&state, gzip_id, "otlp_export_result", index).await["result"],
                "accepted"
            );
        }
        send(&state, gzip_id, json!({"type":"disconnect"}))
            .await
            .unwrap();
        peer.stop().await;
    }
}
#[tokio::test]
async fn official_collector_tls_custom_ca_hostname_and_untrusted_negatives() {
    for transport in ["grpc", "http"] {
        let mut peer = Collector::start(transport, true).await;
        let state = state();
        let id = client(&state, peer.port, peer.params(), vec![empty()])
            .await
            .unwrap();
        let connected = event(&state, id, "otlp_connected", 0).await;
        assert_eq!(connected["tls_verified"], true);
        assert_eq!(connected["server_name"], "localhost");
        send(&state, id, exports()[2].clone()).await.unwrap();
        assert_eq!(
            event(&state, id, "otlp_export_result", 0).await["result"],
            "accepted"
        );
        let connection = state.get_client(id).await.unwrap().connection.unwrap();
        assert_eq!(connection.connected_addr.unwrap().port(), peer.port);
        assert!(connection.local_addr.unwrap().port() > 0);
        send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap();
        let ca_path = peer.dir.path().join("bounded-ca.pem");
        let mut ca = std::fs::read(peer.dir.path().join("cert.pem")).unwrap();
        ca.resize(1024 * 1024, b'\n');
        std::fs::write(&ca_path, &ca).unwrap();
        let mut bounded = peer.params();
        bounded["ca_cert_path"] = json!(ca_path);
        let bounded_id = client(&state, peer.port, bounded.clone(), vec![empty()])
            .await
            .unwrap();
        send(&state, bounded_id, exports()[2].clone())
            .await
            .unwrap();
        send(&state, bounded_id, json!({"type":"disconnect"}))
            .await
            .unwrap();
        ca.push(b'\n');
        std::fs::write(&ca_path, &ca).unwrap();
        let error = client(&state, peer.port, bounded, vec![empty()])
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("CA file must be 1..1048576 bytes"));
        let mut directory = peer.params();
        directory["ca_cert_path"] = json!(peer.dir.path());
        let error = client(&state, peer.port, directory, vec![empty()])
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("CA path must be a regular file"));
        let mut wrong = peer.params();
        wrong["server_name"] = json!("wrong.example");
        assert!(client(&state, peer.port, wrong, vec![empty()])
            .await
            .is_err());
        assert!(client(
            &state,
            peer.port,
            json!({"transport":transport,"tls":true,"server_name":"localhost"}),
            vec![empty()]
        )
        .await
        .is_err());
        peer.stop().await;
    }
}

#[tokio::test]
async fn automatic_exports_stop_after_four_followups_and_manual_commands_still_work() {
    let state = state();
    let (server_id, port) = server(
        &state,
        vec![json!({"event_pattern":"otlp_export","handler":{"type":"static","actions":[{"type":"accept_otlp"}]}})],
    )
    .await;
    let action = exports()[2].clone();
    let code = format!(
        "import json,sys\nd=json.load(sys.stdin)\na=json.loads(r'''{action}''')\nn=int(d['client'].get('memory') or '0')+1\nprint(json.dumps({{'actions':[{{'type':'set_memory','value':str(n)}},a]}}))"
    );
    let id = client(
        &state,
        port,
        json!({"tls":false}),
        vec![
            json!({"event_pattern":"otlp_connected","handler":{"type":"static","actions":[action]}}),
            json!({"event_pattern":"otlp_export_result","handler":{"type":"script","language":"python","code":code}}),
            empty(),
        ],
    )
    .await
    .unwrap();
    event(&state, id, "otlp_export_result", 4).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.get_memory_for_client(id).await.as_deref() != Some("5") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(300),
        event(&state, id, "otlp_export_result", 5),
    )
    .await
    .is_err());
    assert!(matches!(
        send(&state, id, json!({"type":"wait_for_more"}))
            .await
            .unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    state.remove_server(server_id).await.unwrap();
}
#[tokio::test]
async fn netget_pair_all_signals_partial_refusal_retry_info_and_script_memory() {
    let state = state();
    let code="import json,sys\ne=json.load(sys.stdin)['event']\na=[{'type':'accept_otlp'}]\nif e['service_name']=='partial': a=[{'type':'accept_otlp_partially','rejected':1,'error_message':'one item refused'}]\nif e['service_name']=='throttled': a=[{'type':'reject_otlp','code':429,'message':'slow down','retry_after_secs':7}]\nif e['service_name']=='permanent': a=[{'type':'reject_otlp','code':413,'message':'too large'}]\nprint(json.dumps({'actions':a}))";
    let (server_id,port)=server(&state,vec![json!({"event_pattern":"otlp_export","handler":{"type":"script","language":"python","code":code}})]).await;
    for transport in ["grpc", "http"] {
        let action = exports()[2].clone();
        let followup = json!({"event_pattern":"otlp_export_result","handler":{"type":"script","language":"python","code":"import json,sys\nd=json.load(sys.stdin)\nassert d['client']['memory']=='OTLP marker'\nprint(json.dumps({'actions':[]}))"}});
        let id=client(&state,port,json!({"transport":transport,"tls":false,"gzip":true}),vec![json!({"event_pattern":"otlp_connected","handler":{"type":"static","actions":[{"type":"set_memory","value":"OTLP marker"},action]}}),followup,empty()]).await.unwrap();
        assert_eq!(
            event(&state, id, "otlp_export_result", 0).await["result"],
            "accepted"
        );
        assert_eq!(
            state.get_memory_for_client(id).await.as_deref(),
            Some("OTLP marker")
        );
        for (index, service) in ["partial", "throttled", "permanent"]
            .into_iter()
            .enumerate()
        {
            let mut action = exports()[1].clone();
            action["service_name"] = json!(service);
            send(&state, id, action).await.unwrap();
            let result = event(&state, id, "otlp_export_result", index + 1).await;
            match service {
                "partial" => {
                    assert_eq!(result["result"], "partial_success");
                    assert_eq!(result["rejected"], 1);
                    assert_eq!(result["retryable"], false);
                }
                "throttled" => {
                    assert_eq!(result["retryable"], true);
                    assert_eq!(result["retry_after_secs"], 7);
                }
                _ => assert_eq!(result["retryable"], false),
            }
        }
        send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap();
    }
    state.remove_server(server_id).await.unwrap();
}
#[test]
fn typed_builder_bounds_and_no_payload_escape_hatch() {
    for definition in OtlpClientProtocol.get_sync_actions() {
        OtlpClientProtocol
            .execute_action(definition.example)
            .unwrap();
    }
    let base = exports()[0].clone();
    let mut invalid = Vec::new();
    let mut v = base.clone();
    v["spans"] = json!(vec![base["spans"][0].clone(); 129]);
    invalid.push(v);
    let mut v = base.clone();
    v["spans"][0]["name"] = json!("x".repeat(257));
    invalid.push(v);
    let mut v = base.clone();
    v["spans"][0]["trace_id"] = json!("00".repeat(16));
    invalid.push(v);
    let mut v = base.clone();
    v["spans"][0]["span_id"] = json!("invalid");
    invalid.push(v);
    let mut v = base.clone();
    v["spans"][0]["end_time_unix_nano"] = json!(1);
    invalid.push(v);
    let mut v = base.clone();
    v["spans"][0]["kind"] = json!("bogus");
    invalid.push(v);
    let mut v = base.clone();
    v["resource_attributes"] = json!({"service.name":"duplicate"});
    invalid.push(v);
    let mut v = base.clone();
    v["spans"][0]["attributes"] = json!({"nested":[1,2]});
    invalid.push(v);
    let mut v = base.clone();
    v["spans"][0]["attributes"] = json!(std::collections::BTreeMap::from_iter(
        (0..33).map(|i| (format!("a{i}"), json!(i)))
    ));
    invalid.push(v);
    let mut v = exports()[2].clone();
    v["logs"][0]["body"] = json!("x".repeat(4097));
    invalid.push(v);
    let mut v = exports()[2].clone();
    v["logs"][0]["severity_number"] = json!(25);
    invalid.push(v);
    let mut v = exports()[2].clone();
    v["logs"][0].as_object_mut().unwrap().remove("span_id");
    invalid.push(v);
    let mut v = exports()[1].clone();
    v["data_points"][0]["value"] = json!(u64::MAX);
    invalid.push(v);
    let mut v = exports()[1].clone();
    v["data_points"][0]["time_unix_nano"] = json!(0);
    invalid.push(v);
    for v in invalid {
        assert!(wire::build(&v).is_err(), "accepted invalid typed data: {v}");
    }
    assert!(OtlpClientProtocol
        .execute_action(json!({"type":"export_otlp","protobuf":"AA=="}))
        .is_err());
    let attributes =
        serde_json::Map::from_iter((0..32).map(|i| (format!("key{i}"), json!("x".repeat(1024)))));
    let mut large = base.clone();
    let mut span = base["spans"][0].clone();
    span["attributes"] = Value::Object(attributes);
    large["spans"] = json!(vec![span; 128]);
    assert!(wire::build(&large)
        .unwrap_err()
        .to_string()
        .contains("1 MiB"));
}
#[tokio::test]
async fn manual_handlers_capacity_and_disconnect_remain_responsive() {
    let state = state();
    let (server_id,port)=server(&state,vec![json!({"event_pattern":"otlp_export","handler":{"type":"static","actions":[{"type":"accept_otlp"}]}})]).await;
    let id = client(
        &state,
        port,
        json!({"tls":false}),
        vec![
            json!({"event_pattern":"otlp_export_result","handler":{"type":"manual"}}),
            empty(),
        ],
    )
    .await
    .unwrap();
    for expected in 1..=16 {
        send(&state, id, exports()[2].clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.list_intercepts().await.len() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    assert!(send(&state, id, exports()[2].clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("busy"));
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.list_intercepts().await.is_empty() || state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(server_id).await.unwrap();
}

#[tokio::test]
async fn client_export_deadline_idle_and_removal_release_parked_rpc() {
    let state = state();
    let (server_id, port) = server(
        &state,
        vec![json!({"event_pattern":"otlp_export","handler":{"type":"manual"}})],
    )
    .await;
    let id = client(
        &state,
        port,
        json!({"tls":false,"export_timeout_secs":1}),
        vec![empty()],
    )
    .await
    .unwrap();
    match send(&state, id, exports()[2].clone()).await {
        Err(error) => {
            let error = error.to_string().to_lowercase();
            assert!(
                error.contains("deadline") || error.contains("timeout"),
                "{error}"
            );
            event(&state, id, "otlp_export_error", 0).await;
        }
        Ok(ClientSendOutcome::Executed { .. }) => {
            let result = event(&state, id, "otlp_export_result", 0).await;
            assert!(matches!(result["grpc_code"].as_i64(), Some(1 | 4)));
            assert_eq!(result["retryable"], true);
        }
        other => panic!("unexpected deadline outcome: {other:?}"),
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await.unwrap();
    let id = client(&state, port, json!({"tls":false}), vec![empty()])
        .await
        .unwrap();
    let cloned = state.clone();
    let task = tokio::spawn(async move { send(&cloned, id, exports()[2].clone()).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.list_intercepts().await.len() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let id = client(
        &state,
        port,
        json!({"tls":false,"idle_timeout_secs":1}),
        vec![empty()],
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(server_id).await.unwrap();
}

#[tokio::test]
async fn client_connect_deadline_closes_stalled_tls_peer() {
    use tokio::io::AsyncReadExt;
    for transport in ["http", "grpc"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let mut total = 0;
            loop {
                let n = tokio::time::timeout(Duration::from_secs(4), socket.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                if n == 0 {
                    return total;
                }
                total += n;
            }
        });
        let state = state();
        let error = client(
            &state,
            port,
            json!({"transport":transport,"tls":true,"connect_timeout_secs":1}),
            vec![empty()],
        )
        .await
        .unwrap_err();
        let error = format!("{error:#}").to_lowercase();
        assert!(
            error.contains("deadline") || error.contains("timeout"),
            "{error}"
        );
        assert!(peer.await.unwrap() > 0);
    }
}

#[tokio::test]
async fn http_response_exact_limit_plus_one_plain_gzip_and_invalid_payload() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::TokioIo;
    use opentelemetry_proto::tonic::collector::logs::v1::{
        ExportLogsPartialSuccess, ExportLogsServiceResponse,
    };
    use prost::Message;
    const LIMIT: usize = 4 * 1024 * 1024;
    struct Guard(tokio::task::JoinHandle<()>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    for (gzip, over, invalid) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _guard = Guard(tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = hyper::service::service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| async move {
                    http_body_util::Limited::new(request.into_body(), 1024 * 1024)
                        .collect()
                        .await
                        .unwrap();
                    let mut response = ExportLogsServiceResponse {
                        partial_success: Some(ExportLogsPartialSuccess {
                            rejected_log_records: 0,
                            error_message: "r".repeat(LIMIT - 16),
                        }),
                    };
                    while response.encoded_len() < LIMIT + usize::from(over) {
                        response
                            .partial_success
                            .as_mut()
                            .unwrap()
                            .error_message
                            .push('r');
                    }
                    let mut body = if invalid {
                        vec![255]
                    } else {
                        response.encode_to_vec()
                    };
                    if gzip {
                        use std::io::Write;
                        let mut encoder = flate2::write::GzEncoder::new(
                            Vec::new(),
                            flate2::Compression::default(),
                        );
                        encoder.write_all(&body).unwrap();
                        body = encoder.finish().unwrap();
                    }
                    let mut reply = hyper::Response::builder()
                        .header("content-type", "application/x-protobuf")
                        .body(Full::new(Bytes::from(body)))
                        .unwrap();
                    if gzip {
                        reply.headers_mut().insert(
                            "content-encoding",
                            hyper::header::HeaderValue::from_static("gzip"),
                        );
                    }
                    Ok::<_, std::convert::Infallible>(reply)
                },
            );
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        }));
        let state = state();
        let id = client(
            &state,
            port,
            json!({"transport":"http","tls":false}),
            vec![empty()],
        )
        .await
        .unwrap();
        if over || invalid {
            assert!(send(&state, id, exports()[2].clone()).await.is_err());
            event(&state, id, "otlp_export_error", 0).await;
        } else {
            send(&state, id, exports()[2].clone()).await.unwrap();
            let result = event(&state, id, "otlp_export_result", 0).await;
            assert_eq!(result["result"], "partial_success");
            assert_eq!(result["retryable"], false);
            assert_eq!(result["message"].as_str().unwrap().len(), 512);
        }
        state.remove_client(id).await.unwrap();
    }
}
