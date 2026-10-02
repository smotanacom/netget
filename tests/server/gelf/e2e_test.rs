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
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "gelf".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some("This instruction must not trigger default model calls".into()),
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
    .expect("GELF bind");
    (state, id, addr)
}
pub(super) async fn logs(
    state: &AppState,
    id: ServerId,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let logs = state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await;
            if logs.len() >= count {
                break logs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("GELF access log")
}

fn wire(short: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"version":"1.1","host":"demo","short_message":short,"timestamp":1700000000.25,"level":6,"_service":"api"})).unwrap()
}
#[tokio::test]
async fn both_transports_collect_structured_without_model_and_stop_releases_socket() {
    for transport in ["udp", "tcp"] {
        let (state, id, addr) = start(None, Some(json!({"transport":transport}))).await;
        if transport == "tcp" {
            let mut sender = TcpStream::connect(addr).await.unwrap();
            let a = wire("温度");
            sender.write_all(&a[..10]).await.unwrap();
            assert!(state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .is_empty());
            let mut tail = a[10..].to_vec();
            tail.push(0);
            tail.extend_from_slice(&wire("coalesced"));
            tail.push(0);
            sender.write_all(&tail).await.unwrap();
            let e = logs(&state, id, 2).await;
            let first = e
                .iter()
                .find(|e| e.request["message"]["short_message"] == "温度")
                .expect("fragmented message observed");
            assert_eq!(
                first.request["message"]["additional_fields"]["service"],
                "api"
            );
            assert!(e
                .iter()
                .any(|e| e.request["message"]["short_message"] == "coalesced"));
            state.remove_server(id).await;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), sender.read(&mut [0; 1]))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            tokio::net::TcpListener::bind(addr).await.unwrap();
        } else {
            let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            sender.send_to(&wire("温度"), addr).await.unwrap();
            let e = logs(&state, id, 1).await;
            assert_eq!(e[0].request["message"]["short_message"], "温度");
            assert_eq!(e[0].request["transport"], "udp");
            state.remove_server(id).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match tokio::net::UdpSocket::bind(addr).await {
                        Ok(socket) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                            tokio::task::yield_now().await
                        }
                        Err(error) => panic!("UDP socket release: {error}"),
                    }
                }
            })
            .await
            .expect("stopped UDP collector releases socket");
        }
    }
}
#[tokio::test]
async fn tcp_malformed_oversize_partial_and_compressed_frames_close_only_sender() {
    let (state, id, addr) = start(None, Some(json!({"transport":"tcp"}))).await;
    for bytes in [
        b"{}\0".to_vec(),
        vec![b'x'; netget::server::gelf::codec::MAX_MESSAGE_BYTES + 1],
        b"partial".to_vec(),
        vec![0x1f, 0x8b, 0],
    ] {
        let mut peer = TcpStream::connect(addr).await.unwrap();
        peer.write_all(&bytes).await.unwrap();
        peer.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
    assert!(logs(&state, id, 4)
        .await
        .iter()
        .all(|e| e.event_type == "gelf_invalid_stream"));
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(&[wire("healthy"), vec![0]].concat())
        .await
        .unwrap();
    assert!(logs(&state, id, 5)
        .await
        .iter()
        .any(|e| e.event_type == "gelf_message"));
    state.remove_server(id).await;
}
#[tokio::test]
async fn udp_bad_datagrams_do_not_poison_collector_and_missing_timestamp_defaults() {
    let (state, id, addr) = start(None, None).await;
    let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for bytes in [vec![], b"{}".to_vec(), vec![b'x'; 8193], vec![0x1e, 0x0f]] {
        sender.send_to(&bytes, addr).await.unwrap();
    }
    logs(&state, id, 4).await;
    sender
        .send_to(
            br#"{"version":"1.1","host":"demo","short_message":"defaults"}"#,
            addr,
        )
        .await
        .unwrap();
    let e = logs(&state, id, 5).await;
    let m = &e
        .iter()
        .find(|e| e.event_type == "gelf_message")
        .unwrap()
        .request["message"];
    assert_eq!(m["level"], 1);
    assert!(m["timestamp"].as_f64().unwrap() > 1700000000.0);
    state.remove_server(id).await;
}
#[tokio::test]
async fn explicit_static_script_and_manual_handlers_run_with_default_fallback_disabled() {
    for transport in ["udp", "tcp"] {
        for handler in [
            json!({"type":"static","actions":[]}),
            json!({"type":"script","language":"python","code":"import json,sys\nx=json.load(sys.stdin)\nassert x['event']['message']['short_message']=='handler'\nprint(json.dumps({'actions':[],'memory':{'observed':True}}))"}),
        ] {
            let (state, id, addr) = start(
                Some(vec![
                    json!({"event_pattern":"gelf_message","handler":handler}),
                ]),
                Some(json!({"transport":transport})),
            )
            .await;
            if transport == "tcp" {
                let mut peer = TcpStream::connect(addr).await.unwrap();
                peer.write_all(&[wire("handler"), vec![0]].concat())
                    .await
                    .unwrap();
            } else {
                let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                peer.send_to(&wire("handler"), addr).await.unwrap();
            }
            assert!(logs(&state, id, 1).await[0].response.is_empty());
            state.remove_server(id).await;
        }
    }
    let (state, id, addr) = start(
        Some(vec![
            json!({"event_pattern":"gelf_message","handler":{"type":"manual","timeout_secs":300}}),
        ]),
        Some(json!({"transport":"tcp"})),
    )
    .await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(&[wire("parked"), vec![0]].concat())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(state.list_intercepts().await.is_empty());
}
#[tokio::test]
async fn netget_pairs_both_transports_with_compression_and_chunks() {
    for (transport, compression) in [
        ("udp", "none"),
        ("udp", "gzip"),
        ("udp", "zlib"),
        ("tcp", "none"),
    ] {
        let (state, id, addr) = start(None, Some(json!({"transport":transport}))).await;
        let (tx, _) = mpsc::unbounded_channel();
        let short = "full typed log 温度 ".repeat(100);
        let client=netget::cli::management::ClientForm{protocol:"gelf".into(),remote_addr:Some(addr.to_string()),instruction:Some(String::new()),startup_params:Some(json!({"transport":transport,"compression":compression,"chunk_size":100})),event_handlers:Some(vec![json!({"event_pattern":"gelf_connected","handler":{"type":"static","actions":[{"type":"send_gelf_message","message":{"host":"pair","short_message":short,"timestamp":123,"level":6,"additional_fields":{"n":42}}}]}})]),..Default::default()}.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1"),tx).await.unwrap();
        let e = logs(&state, id, 1).await;
        assert_eq!(e[0].request["message"]["short_message"], short);
        assert_eq!(e[0].request["message"]["additional_fields"]["n"], 42);
        state.remove_client(client).await;
        state.remove_server(id).await;
    }
}

#[tokio::test]
async fn explicit_model_opt_in_runs_the_standard_dispatcher_on_both_transports(
) -> crate::server::helpers::E2EResult<()> {
    use crate::server::helpers::{start_netget_server, NetGetConfig};
    for transport in ["udp", "tcp"] {
        let config=NetGetConfig::new("listen on port {AVAILABLE_PORT} via gelf. Summarize messages.").with_mock(|mock| {
            mock.on_instruction_containing("via gelf").respond_with_actions(json!([{"type":"open_server","base_stack":"gelf","port":0,"instruction":"Summarize messages","startup_params":{"transport":transport,"llm_fallback":true}}])).expect_calls(1).and()
            .on_event("gelf_message").respond_with_actions_from_event(|event| {assert_eq!(event["message"]["short_message"],"model");json!([{"type":"collect_gelf_message"}])}).expect_calls(1).and()
        });
        let server = start_netget_server(config).await?;
        if transport == "tcp" {
            let mut peer = TcpStream::connect(("127.0.0.1", server.port)).await?;
            peer.write_all(&[wire("model"), vec![0]].concat()).await?;
        } else {
            let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
            peer.send_to(&wire("model"), ("127.0.0.1", server.port))
                .await?;
        }
        server.wait_for_mocks(20).await;
        server.verify_mocks().await?;
        server.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn handlers_write_standard_server_memory_and_next_event_observes_it() {
    let code="import json,sys\nx=json.load(sys.stdin)\nm=x['event']['message']['short_message']\nassert x['server']['memory']==('' if m=='first' else 'first')\nprint(json.dumps({'actions':[{'type':'set_memory','value':m},{'type':'collect_gelf_message'}]}))";
    let (state,id,addr)=start(Some(vec![json!({"event_pattern":"gelf_message","handler":{"type":"script","language":"python","code":code}})]),Some(json!({"transport":"tcp"}))).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(&[wire("first"), vec![0]].concat())
        .await
        .unwrap();
    logs(&state, id, 1).await;
    assert_eq!(state.get_server(id).await.unwrap().memory, "first");
    peer.write_all(&[wire("second"), vec![0]].concat())
        .await
        .unwrap();
    logs(&state, id, 2).await;
    assert_eq!(state.get_server(id).await.unwrap().memory, "second");
    state.remove_server(id).await;
}

#[tokio::test]
async fn trickled_partial_gelf_frame_does_not_reset_absolute_read_deadline() {
    let (state, id, addr) = start(None, Some(json!({"transport":"tcp"}))).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"a").await.unwrap();
    for bytes in [1, 2] {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state
                    .get_server(id)
                    .await
                    .unwrap()
                    .connections
                    .values()
                    .any(|c| c.bytes_received >= bytes)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        if bytes == 1 {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(29)).await;
            tokio::time::resume();
            peer.write_all(b"b").await.unwrap();
        }
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(logs(&state, id, 1).await[0].response[0]["error"]
        .as_str()
        .unwrap()
        .contains("deadline"));
    state.remove_server(id).await;
}

#[tokio::test]
async fn tcp_connection_cap_refuses_excess_and_closed_peer_returns_slot() {
    let (state, id, addr) = start(None, Some(json!({"transport":"tcp"}))).await;
    let mut peers = Vec::new();
    for _ in 0..netget::server::accept_bounded::DEFAULT_MAX_CONNECTIONS {
        peers.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.get_server(id).await.unwrap().connections.len() != peers.len() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut excess = TcpStream::connect(addr).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), excess.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    drop(peers.pop());
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.get_server(id).await.unwrap().connections.len() != peers.len() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut replacement = TcpStream::connect(addr).await.unwrap();
    replacement
        .write_all(&[wire("cap recovered"), vec![0]].concat())
        .await
        .unwrap();
    logs(&state, id, 1).await;
    state.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), replacement.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}
