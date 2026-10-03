use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};
pub(super) async fn start(
    handlers: Option<Vec<Value>>,
    params: Option<Value>,
) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "loki".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some("Default collection must not call the model".into()),
        event_handlers: handlers,
        startup_params: params,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, addr)
}
pub(super) async fn request(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &str,
    body: &[u8],
) -> (u16, String, String) {
    let default_ct = if headers.to_ascii_lowercase().contains("content-type:") {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let headers = format!("{default_ct}{headers}");
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(format!("{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n{headers}\r\n",body.len()).as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let wire = String::from_utf8(bytes).unwrap();
    let (head, body) = wire.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, body.into(), head.into())
}
pub(super) async fn logs(
    state: &AppState,
    id: ServerId,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let entries = state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await;
            if entries.len() >= count {
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
pub(super) const TARGET: &str = "/loki/api/v1/push";

pub(super) const BODY: &[u8] =
    br#"{"streams":[{"stream":{"app":"test"},"values":[["123","hello",{"trace_id":"x"}]]}]}"#;
#[tokio::test]
async fn connection_cap_refuses_and_malformed_peer_releases_slot() {
    let (state, id, addr) = start(None, None).await;
    let mut peers = Vec::new();
    for _ in 0..netget::server::accept_bounded::DEFAULT_MAX_CONNECTIONS {
        let mut p = TcpStream::connect(addr).await.unwrap();
        p.write_all(b"POST ").await.unwrap();
        peers.push(p);
    }
    let mut refused = TcpStream::connect(addr).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), refused.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 503"));
    // A capacity refusal remains a valid Loki API error for the native emitter.
    let (tx, _) = mpsc::unbounded_channel();
    let client = netget::cli::management::ClientForm {
        protocol: "loki".into(),
        remote_addr: Some(addr.to_string()),
        instruction: Some(String::new()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.has_client_handle(client).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let outcome = state
        .send_to_client(
            client,
            json!({"type":"push_loki_entries","batch":super::codec_test::batch("json")}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        netget::state::client_handles::ClientSendOutcome::Executed { .. }
    ));
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(client.as_u32())), None)
                .await
                .into_iter()
                .find(|e| e.event_type == "loki_push_response")
            {
                break e;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(event.request["status"], 503);
    assert_eq!(event.request["retry_after_seconds"], 5);
    assert_eq!(event.request["error"]["message"], "connection capacity");
    state.remove_client(client).await;

    let mut malformed = peers.pop().unwrap();
    let _ = malformed
        .write_all(b"/ HTTP/1.1\r\nMalformed\r\n\r\n")
        .await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), malformed.read_to_end(&mut bytes))
        .await
        .unwrap();
    assert_eq!(request(addr, "POST", TARGET, "", BODY).await.0, 204);
    state.remove_server(id).await;
    for mut p in peers {
        let closed = tokio::time::timeout(Duration::from_secs(5), p.read(&mut [0; 1]))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0)) || closed.is_err());
    }
}

#[tokio::test]
async fn server_stop_cancels_parked_handler_socket_intercept_and_listener() {
    let (state, id, addr) = start(
        Some(vec![
            json!({"event_pattern":"loki_push","handler":{"type":"manual","timeout_secs":300}}),
        ]),
        None,
    )
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("POST {TARGET} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",BODY.len(),std::str::from_utf8(BODY).unwrap())
                .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.read(&mut [0; 1]))
            .await
            .is_err()
    );
    assert!(!state.list_intercepts().await.is_empty());
    state.remove_server(id).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut [0; 1]))
        .await
        .unwrap();
    assert!(matches!(closed, Ok(0)) || closed.is_err());
    assert!(state.list_intercepts().await.is_empty());
    assert!(TcpStream::connect(addr).await.is_err());
}
#[tokio::test]
async fn body_and_gzip_limits_reject_without_dispatch() {
    let (state, id, addr) = start(None, None).await;
    assert_eq!(
        request(
            addr,
            "POST",
            TARGET,
            "",
            &vec![b'x'; netget::server::loki::codec::MAX_BODY_BYTES + 1]
        )
        .await
        .0,
        413
    );
    use std::io::Write;
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&vec![b'x'; netget::server::loki::codec::MAX_BODY_BYTES + 1])
        .unwrap();
    assert_eq!(
        request(
            addr,
            "POST",
            TARGET,
            "Content-Encoding: gzip\r\n",
            &z.finish().unwrap()
        )
        .await
        .0,
        413
    );
    assert_eq!(
        request(
            addr,
            "POST",
            TARGET,
            "Content-Encoding: gzip\r\n",
            b"not gzip"
        )
        .await
        .0,
        400
    );
    assert!(state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .is_empty());
    state.remove_server(id).await;
}
#[tokio::test]
async fn absolute_header_deadline_closes_slow_connection() {
    deadline_case(true).await;
}
#[tokio::test]
async fn absolute_body_deadline_returns_408_before_closing() {
    deadline_case(false).await;
}
async fn deadline_case(header: bool) {
    // Separate runtimes avoid reusing Tokio's advanced test clock with Hyper's
    // std::Instant-based header timer on the next connection.
    let (state, id, addr) = start(None, None).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let prefix = if header {
        "POST /loki/api/v1/push HTTP/1.1\r\nHost: "
    } else {
        "POST /loki/api/v1/push HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\nx"
    };
    stream.write_all(prefix.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .unwrap();
    if !header {
        assert!(
            String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 408"),
            "body deadline wire: {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }
    assert!(state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .is_empty());
    state.remove_server(id).await;
}
#[tokio::test]
async fn all_carriers_collect_typed_streams_without_default_model_calls() {
    let (state, id, addr) = start(None, None).await;
    for kind in ["json", "gzip_json", "snappy_protobuf"] {
        let b = super::codec_test::batch(kind);
        let body = netget::server::loki::codec::encode_batch(&b).unwrap();
        let headers = match kind {
            "json" => "Content-Type: application/json\r\n",
            "gzip_json" => "Content-Type: application/json\r\nContent-Encoding: gzip\r\n",
            _ => "Content-Type: application/x-protobuf\r\n",
        };
        assert_eq!(
            request(
                addr,
                "POST",
                TARGET,
                &format!("{headers}X-Scope-OrgID: tenant-one\r\n"),
                &body
            )
            .await
            .0,
            204
        );
    }
    let e = logs(&state, id, 3).await;
    for e in e {
        assert_eq!(e.request["tenant_id"], "tenant-one");
        assert_eq!(
            e.request["streams"][0]["entries"][0]["timestamp_ns"],
            1700000000000000123i64
        );
        assert_eq!(
            e.request["streams"][0]["entries"][0]["structured_metadata"]["trace_id"],
            "0123名"
        );
    }
    state.remove_server(id).await;
}
#[tokio::test]
async fn route_tenant_auth_encoding_and_malformed_batch_errors_never_dispatch_or_leak_tokens() {
    let (state, id, addr) = start(
        None,
        Some(json!({"require_tenant":true,"auth_token":"secret-token"})),
    )
    .await;
    let ok="Content-Type: application/json\r\nAuthorization: Bearer secret-token\r\nX-Scope-OrgID: tenant-one\r\n";
    assert_eq!(request(addr, "POST", TARGET, ok, BODY).await.0, 204);
    for(status,method,target,headers,body)in [(401,"POST",TARGET,"",BODY),(401,"POST",TARGET,"Authorization: Bearer secret-token\r\n",BODY),(400,"POST",TARGET,"Authorization: Bearer secret-token\r\nX-Scope-OrgID: bad|tenant\r\n",BODY),(400,"POST",TARGET,"Authorization: Bearer secret-token\r\nX-Scope-OrgID: one\r\nX-Scope-OrgID: two\r\n",BODY),(404,"POST","/wrong",ok,BODY),(405,"GET",TARGET,ok,BODY),(400,"POST","/loki/api/v1/push?x=1",ok,BODY),(415,"POST",TARGET,"Authorization: Bearer secret-token\r\nX-Scope-OrgID: one\r\nContent-Type: text/plain\r\n",BODY),(415,"POST",TARGET,"Authorization: Bearer secret-token\r\nX-Scope-OrgID: one\r\nContent-Encoding: br\r\n",BODY),(400,"POST",TARGET,ok,b"{}" as &[u8])]{assert_eq!(request(addr,method,target,headers,body).await.0,status);}
    let e = logs(&state, id, 1).await;
    assert_eq!(e.len(), 1);
    assert!(!serde_json::to_string(&e[0].request)
        .unwrap()
        .contains("secret-token"));
    state.remove_server(id).await;
}
#[tokio::test]
async fn explicit_rejection_blocked_status_mixed_action_failure_and_memory_refresh() {
    for status in [260, 400, 422, 429, 503] {
        let action = json!({"type":"reject_loki_entries","status":status,"message":"rejected","retry_after_seconds":if [429,503].contains(&status){Some(3)}else{None}});
        let mut action = action;
        action.as_object_mut().unwrap().retain(|_, v| !v.is_null());
        let (state, id, addr) = start(
            Some(vec![
                json!({"event_pattern":"loki_push","handler":{"type":"static","actions":[action]}}),
            ]),
            None,
        )
        .await;
        let r = request(addr, "POST", TARGET, "", BODY).await;
        assert_eq!(r.0, status);
        assert_eq!(r.1, "rejected");
        if [429, 503].contains(&status) {
            assert!(r.2.to_ascii_lowercase().contains("retry-after: 3"));
        }
        state.remove_server(id).await;
    }
    let script="import json,sys\njson.load(sys.stdin)\nprint(json.dumps({'actions':[{'type':'accept_loki_entries'},{'type':'unknown_loki'}]}))";
    let(state,id,addr)=start(Some(vec![json!({"event_pattern":"loki_push","handler":{"type":"script","language":"python","code":script}})]),None).await;
    assert_eq!(request(addr, "POST", TARGET, "", BODY).await.0, 503);
    assert!(logs(&state, id, 1)
        .await
        .iter()
        .any(|e| e.event_type == "loki_handler_failed"));
    state.remove_server(id).await;
    let script="import json,sys\nx=json.load(sys.stdin)\nn=int(x['server']['memory'] or '0')+1\nprint(json.dumps({'actions':[{'type':'set_memory','value':str(n)},{'type':'accept_loki_entries'}]}))";
    let(state,id,addr)=start(Some(vec![json!({"event_pattern":"loki_push","handler":{"type":"script","language":"python","code":script}})]),None).await;
    for _ in 0..2 {
        assert_eq!(request(addr, "POST", TARGET, "", BODY).await.0, 204);
    }
    assert_eq!(state.get_memory(id).await.unwrap(), "2");
    state.remove_server(id).await;
}
#[tokio::test]
async fn native_pair_followup_depth_and_common_client_memory() {
    let (state, server, addr) = start(None, None).await;
    let (tx, _) = mpsc::unbounded_channel();
    let b = serde_json::to_value(super::codec_test::batch("snappy_protobuf")).unwrap();
    let code=format!("import json,sys\nx=json.load(sys.stdin)\nn=int(x['client']['memory'] or '0')+1\nassert x['event']['status']==204\nprint(json.dumps({{'actions':[{{'type':'set_memory','value':str(n)}},{{'type':'push_loki_entries','batch':json.loads({})}}]}}))",serde_json::to_string(&b.to_string()).unwrap());
    let client=netget::cli::management::ClientForm{protocol:"loki".into(),remote_addr:Some(addr.to_string()),instruction:Some(String::new()),event_handlers:Some(vec![json!({"event_pattern":"loki_connected","handler":{"type":"static","actions":[{"type":"push_loki_entries","batch":b}]}}),json!({"event_pattern":"loki_push_response","handler":{"type":"script","language":"python","code":code}})]),..Default::default()}.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1"),tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while state.get_client(client).await.unwrap().memory != "8" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(logs(&state, server, 8).await.len(), 8);
    state.remove_client(client).await;
    state.remove_server(server).await;
}
#[tokio::test]
async fn oversized_and_excess_header_counts_close_without_dispatch() {
    let (state, id, addr) = start(None, None).await;
    for header in [
        format!(
            "X-Large: {}\r\n",
            "x".repeat(netget::server::loki::MAX_HEADER_BYTES)
        ),
        (0..netget::server::loki::MAX_HEADERS + 1)
            .map(|i| format!("X-{i}: y\r\n"))
            .collect::<String>(),
    ] {
        let mut peer = TcpStream::connect(addr).await.unwrap();
        let _ = peer
            .write_all(
                format!("POST {TARGET} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n{header}\r\n")
                    .as_bytes(),
            )
            .await;
        let mut bytes = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), peer.read_to_end(&mut bytes))
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 204"));
    }
    assert!(state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .is_empty());
    state.remove_server(id).await;
}
#[tokio::test]
async fn explicit_llm_opt_in_uses_standard_typed_event_dispatcher(
) -> crate::server::helpers::E2EResult<()> {
    use crate::server::helpers::{start_netget_server, NetGetConfig};
    let config=NetGetConfig::new("listen on port {AVAILABLE_PORT} via loki. Review logs.").with_mock(|mock|{mock.on_instruction_containing("via loki").respond_with_actions(json!([{"type":"open_server","base_stack":"loki","port":0,"instruction":"Review logs","startup_params":{"llm_fallback":true}}])).expect_calls(1).and().on_event("loki_push").respond_with_actions_from_event(|event|{assert_eq!(event["streams"][0]["labels"]["app"],"test");assert_eq!(event["streams"][0]["entries"][0]["timestamp_ns"],123);assert_eq!(event["streams"][0]["entries"][0]["structured_metadata"]["trace_id"],"x");json!([{"type":"accept_loki_entries"}])}).expect_calls(1).and()});
    let server = start_netget_server(config).await?;
    assert_eq!(
        request(
            (
                "127.0.0.1".parse::<std::net::IpAddr>().unwrap(),
                server.port
            )
                .into(),
            "POST",
            TARGET,
            "",
            BODY
        )
        .await
        .0,
        204
    );
    server.wait_for_mocks(20).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
