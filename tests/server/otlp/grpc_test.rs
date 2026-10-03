use super::{common, real_client_test};
use netget::server::otlp::MAX_BODY_BYTES;
use opentelemetry_proto::tonic::collector::{
    logs::v1 as logs, metrics::v1 as metrics, trace::v1 as traces,
};
use opentelemetry_proto::tonic::trace::v1::ResourceSpans;
use prost::Message;
use serde_json::json;
use std::time::Duration;
use tonic::{codec::CompressionEncoding, transport::Channel, Code};
async fn channel(port: u16) -> Channel {
    tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
        .unwrap()
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .unwrap()
}
#[tokio::test]
async fn grpc_generated_all_signals_partial_status_and_fail_closed() {
    let state = common::new_state().await;
    let code="import json,sys\ne=json.load(sys.stdin)['event']\nassert e['transport']=='grpc' and e['encoding']=='protobuf'\na=[{'type':'accept_otlp'}]\nif e['signal']=='metrics': a=[{'type':'accept_otlp_partially','rejected':999,'error_message':'bounded partial'}]\nif e['signal']=='logs': a=[{'type':'reject_otlp','code':403,'message':'not allowed'}]\nprint(json.dumps({'actions':a}))";
    let (id,port,_)=common::start(&state,vec![json!({"event_pattern":"otlp_export","handler":{"type":"script","language":"python","code":code}})]).await;
    let session = channel(port).await;
    for gzip in [false, true] {
        let mut client = traces::trace_service_client::TraceServiceClient::new(session.clone())
            .accept_compressed(CompressionEncoding::Gzip);
        if gzip {
            client = client.send_compressed(CompressionEncoding::Gzip);
        }
        let response = client
            .export(traces::ExportTraceServiceRequest::default())
            .await
            .unwrap()
            .into_inner();
        assert!(response.partial_success.is_none());
        let response = metrics::metrics_service_client::MetricsServiceClient::new(session.clone())
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
            .export(metrics::ExportMetricsServiceRequest::default())
            .await
            .unwrap()
            .into_inner();
        let partial = response.partial_success.unwrap();
        assert_eq!(partial.rejected_data_points, 0);
        assert_eq!(partial.error_message, "bounded partial");
        let error = logs::logs_service_client::LogsServiceClient::new(session.clone())
            .export(logs::ExportLogsServiceRequest::default())
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::PermissionDenied);
        assert_eq!(error.message(), "not allowed");
    }
    state.remove_server(id).await.unwrap();
    let (id, port, _) = common::start(&state, vec![common::static_handler(json!([]))]).await;
    let error = traces::trace_service_client::TraceServiceClient::new(channel(port).await)
        .export(traces::ExportTraceServiceRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::Internal);
    assert!(error.message().contains("no decision"));
    state.remove_server(id).await.unwrap();
}
fn sized(size: usize) -> traces::ExportTraceServiceRequest {
    let mut request = traces::ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            schema_url: "x".repeat(size - 16),
            ..Default::default()
        }],
    };
    while request.encoded_len() < size {
        request.resource_spans[0].schema_url.push('x');
    }
    assert_eq!(request.encoded_len(), size);
    request
}
#[tokio::test]
async fn grpc_exact_body_limit_and_gzip_bomb_never_raise_overlimit_event() {
    let state = common::new_state().await;
    let (id, port, _) = common::start(
        &state,
        vec![common::static_handler(json!([{"type":"accept_otlp"}]))],
    )
    .await;
    let channel = channel(port).await;
    for gzip in [false, true] {
        let mut client = traces::trace_service_client::TraceServiceClient::new(channel.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_encoding_message_size(MAX_BODY_BYTES + 1);
        if gzip {
            client = client.send_compressed(CompressionEncoding::Gzip);
        }
        client.export(sized(MAX_BODY_BYTES)).await.unwrap();
        let before = state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Server(id.as_u32())),
                None,
            )
            .await
            .len();
        assert_eq!(
            client
                .export(sized(MAX_BODY_BYTES + 1))
                .await
                .unwrap_err()
                .code(),
            Code::ResourceExhausted
        );
        assert_eq!(
            state
                .list_access_logs_for(
                    Some(netget::state::AccessLogOwner::Server(id.as_u32())),
                    None
                )
                .await
                .len(),
            before
        );
        client
            .export(traces::ExportTraceServiceRequest::default())
            .await
            .unwrap();
    }
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn grpc_parked_deadline_cancellation_and_server_removal_drop_handlers() {
    let state = common::new_state().await;
    let (id, port, _) = common::start(
        &state,
        vec![json!({"event_pattern":"otlp_export","handler":{"type":"manual"}})],
    )
    .await;
    let channel = channel(port).await;
    let mut request = tonic::Request::new(traces::ExportTraceServiceRequest::default());
    request.set_timeout(Duration::from_millis(150));
    let error = traces::trace_service_client::TraceServiceClient::new(channel.clone())
        .export(request)
        .await
        .unwrap_err();
    assert!(matches!(
        error.code(),
        Code::DeadlineExceeded | Code::Cancelled
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let task = tokio::spawn(async move {
        traces::trace_service_client::TraceServiceClient::new(channel)
            .export(traces::ExportTraceServiceRequest::default())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.list_intercepts().await.len() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(state.list_intercepts().await.is_empty());
}

#[tokio::test]
async fn grpc_global_admission_bound_releases_on_cancel_before_decoding() {
    let state = common::new_state().await;
    let (id, port, _) = common::start(
        &state,
        vec![json!({"event_pattern":"otlp_export","handler":{"type":"manual"}})],
    )
    .await;
    let mut tasks = tokio::task::JoinSet::new();
    let mut first = None;
    for _ in 0..4 {
        let channel = channel(port).await;
        for _ in 0..16 {
            let channel = channel.clone();
            let abort = tasks.spawn(async move {
                traces::trace_service_client::TraceServiceClient::new(channel)
                    .export(traces::ExportTraceServiceRequest::default())
                    .await
            });
            if first.is_none() {
                first = Some(abort);
            }
        }
    }
    tokio::time::timeout(Duration::from_secs(8), async {
        while state.list_intercepts().await.len() != 64 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let channel = channel(port).await;
    let mut overflow = traces::trace_service_client::TraceServiceClient::new(channel.clone());
    assert_eq!(
        overflow
            .export(traces::ExportTraceServiceRequest::default())
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    first.unwrap().abort();
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.list_intercepts().await.len() != 63 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tasks.spawn(async move {
        traces::trace_service_client::TraceServiceClient::new(channel)
            .export(traces::ExportTraceServiceRequest::default())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.list_intercepts().await.len() != 64 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(result) = tasks.join_next().await {
            if let Ok(result) = result {
                assert!(result.is_err());
            }
        }
    })
    .await
    .unwrap();
    assert!(state.list_intercepts().await.is_empty());
}

#[tokio::test]
async fn grpc_single_message_guard_rejects_prefix_bombs_and_extra_frames() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let state = common::new_state().await;
    let (id, port, _) = common::start(
        &state,
        vec![common::static_handler(json!([{"type":"accept_otlp"}]))],
    )
    .await;
    for (body, code) in [
        (vec![0, 255, 255, 255, 255], 8),
        (vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 3),
        (Vec::new(), 3),
    ] {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let (mut sender, driver) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(stream))
            .await
            .unwrap();
        let task = tokio::spawn(driver);
        let request=hyper::Request::builder().method("POST").uri(format!("http://127.0.0.1:{port}/opentelemetry.proto.collector.trace.v1.TraceService/Export")).header("content-type","application/grpc").body(Full::new(Bytes::from(body))).unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(
            response.headers()["grpc-status"]
                .to_str()
                .unwrap()
                .parse::<i32>()
                .unwrap(),
            code
        );
        response.into_body().collect().await.unwrap();
        task.abort();
        let _ = task.await;
    }
    assert!(state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Server(id.as_u32())),
            None
        )
        .await
        .is_empty());
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn grpc_message_compression_flag_and_unique_timeout_are_validated() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let state = common::new_state().await;
    let (id, port, _) = common::start(
        &state,
        vec![common::static_handler(json!([{"type":"accept_otlp"}]))],
    )
    .await;
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut sender, driver) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let task = tokio::spawn(driver);
    let compressed = common::gzip(&[]);
    let mut gzip_frame = vec![1];
    gzip_frame.extend_from_slice(&u32::try_from(compressed.len()).unwrap().to_be_bytes());
    gzip_frame.extend_from_slice(&compressed);
    let mut expected = Vec::new();
    for (body, timeouts, code, flag) in [
        (vec![0, 0, 0, 0, 0], vec![], 0, Some(false)),
        (gzip_frame, vec![], 0, Some(true)),
        (vec![0, 0, 0, 0, 0], vec!["30S", "1S"], 3, None),
        (vec![0, 0, 0, 0, 0], vec!["30S,1S"], 3, None),
        (vec![0, 0, 0, 0, 0], vec!["30S"], 0, Some(false)),
    ] {
        let mut request = hyper::Request::builder()
            .method("POST")
            .uri(format!("http://127.0.0.1:{port}/opentelemetry.proto.collector.trace.v1.TraceService/Export"))
            .header("content-type", "application/grpc")
            .header("grpc-encoding", "gzip");
        for timeout in timeouts {
            request = request.header("grpc-timeout", timeout);
        }
        let response = sender
            .send_request(request.body(Full::new(Bytes::from(body))).unwrap())
            .await
            .unwrap();
        let status = response.headers().get("grpc-status").cloned();
        let collected = response.into_body().collect().await.unwrap();
        let status = status.or_else(|| collected.trailers()?.get("grpc-status").cloned());
        assert_eq!(
            status.unwrap().to_str().unwrap().parse::<i32>().unwrap(),
            code
        );
        if let Some(flag) = flag {
            expected.push(flag);
        }
        let mut observed: Vec<_> = state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Server(id.as_u32())),
                None,
            )
            .await
            .into_iter()
            .filter(|log| log.event_type == "otlp_export")
            .map(|log| log.request["compressed"].as_bool().unwrap())
            .collect();
        observed.sort();
        expected.sort();
        assert_eq!(observed, expected);
    }
    task.abort();
    let _ = task.await;
    state.remove_server(id).await.unwrap();
}

#[tokio::test]
async fn independent_otel_cli_and_telemetrygen_grpc_all_signals_and_refusal() {
    let state = common::new_state().await;
    let code="import json,sys\ne=json.load(sys.stdin)['event']\nassert e['transport']=='grpc'\na=[{'type':'accept_otlp'}]\nif e.get('service_name')=='refused': a=[{'type':'reject_otlp','code':403,'message':'independent refusal'}]\nprint(json.dumps({'actions':a}))";
    let (id,port,mut rx)=common::start(&state,vec![json!({"event_pattern":"otlp_export","handler":{"type":"script","language":"python","code":code}})]).await;
    for service in ["grpc-checkout", "refused"] {
        let args: [String; 15] = [
            "span",
            "--endpoint",
            &format!("127.0.0.1:{port}"),
            "--protocol",
            "grpc",
            "--insecure",
            "--service",
            service,
            "--name",
            "grpc charge marker",
            "--timeout",
            "30s",
            "--fail",
            "--verbose",
            "--tp-ignore-env",
        ]
        .map(str::to_owned);
        let (status, text) = real_client_test::run("otel-cli", &args).await;
        if service == "refused" {
            assert_ne!(status, 0);
            assert!(text.contains("PermissionDenied"), "{text}");
        } else {
            assert_eq!(status, 0, "{text}");
        }
    }
    for (signal, count, extra) in [
        ("metrics", "--metrics", vec!["--service", "grpc-billing"]),
        (
            "logs",
            "--logs",
            vec!["--service", "grpc-billing", "--body", "grpc log marker"],
        ),
    ] {
        let mut args = vec![
            signal.to_owned(),
            "--otlp-insecure".into(),
            "--otlp-endpoint".into(),
            format!("127.0.0.1:{port}"),
            "--rate".into(),
            "0".into(),
            count.into(),
            "2".into(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let (status, text) = real_client_test::run("telemetrygen", &args).await;
        assert_eq!(status, 0, "{text}");
    }
    common::wait_for_log(&mut rx, "decision=model_answer items=2", 30).await;
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Server(id.as_u32())),
            None,
        )
        .await;
    let exports: Vec<_> = logs
        .iter()
        .filter(|e| e.event_type == "otlp_export")
        .collect();
    assert!(exports
        .iter()
        .any(|e| e.request["span_names"] == json!(["grpc charge marker"])));
    assert!(exports
        .iter()
        .any(|e| e.request["metric_count"].as_u64().is_some_and(|n| n > 0)));
    assert!(exports
        .iter()
        .any(|e| format!("{}", e.request["log_bodies"]).contains("grpc log marker")));
    state.remove_server(id).await.unwrap();
}
