use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, ServerId};
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};
pub async fn start(auth: bool, idle: u64) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id=ServerForm { protocol:"nut".into(),port:Some(0),host:Some("127.0.0.1".into()),startup_params:Some(json!({"idle_timeout_secs":idle})),event_handlers:Some(vec![
        json!({"event_pattern":"nut_auth","handler":{"type":"static","actions":[{"type":"nut_auth_decision","allowed":auth}]}}),
        json!({"event_pattern":"nut_request","handler":{"type":"static","actions":[{"type":"nut_reply","entries":[{"name":"ups","description":"Rack UPS","value":"OL"}],"value":"OL","ok":true,"types":["STRING:32"]}]}}),
    ]),..Default::default() }.create(&state,tx).await.unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}
async fn line(r: &mut BufReader<TcpStream>) -> String {
    let mut s = String::new();
    tokio::time::timeout(Duration::from_secs(3), r.read_line(&mut s))
        .await
        .unwrap()
        .unwrap();
    s
}
#[tokio::test]
async fn structured_server_session_auth_and_cleanup() {
    let (state, id, addr) = start(true, 300).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    r.get_mut().write_all(b"STARTTLS\nLIST UPS\nGET VAR ups ups.status\nINSTCMD ups test.panel.start\nUSERNAME \"operator\"\nPASSWORD \"secret\"\nSET VAR ups ups.delay.start \"10\"\nINSTCMD ups test.panel.start\nLOGOUT\n").await.unwrap();
    for expected in [
        "ERR FEATURE-NOT-SUPPORTED\n",
        "BEGIN LIST UPS\n",
        "UPS ups \"Rack UPS\"\n",
        "END LIST UPS\n",
        "VAR ups ups.status \"OL\"\n",
        "ERR ACCESS-DENIED\n",
        "OK\n",
        "OK\n",
        "OK\n",
        "OK\n",
        "OK Goodbye\n",
        "",
    ] {
        assert_eq!(line(&mut r).await, expected);
    }
    state.remove_server(id).await;
    assert!(TcpStream::connect(addr).await.is_err());
}
#[tokio::test]
async fn denied_credentials_cannot_execute_commands() {
    let (state, id, addr) = start(false, 300).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    r.get_mut()
        .write_all(b"PASSWORD x\nUSERNAME x\nUSERNAME y\nPASSWORD bad\nINSTCMD ups load.off\n")
        .await
        .unwrap();
    for expected in [
        "ERR USERNAME-REQUIRED\n",
        "OK\n",
        "ERR ALREADY-SET\n",
        "ERR ACCESS-DENIED\n",
        "ERR ACCESS-DENIED\n",
    ] {
        assert_eq!(line(&mut r).await, expected);
    }
    state.remove_server(id).await;
    assert_eq!(line(&mut r).await, "");
}
#[tokio::test]
async fn idle_deadline_and_stop_release_existing_sockets() {
    let (state, id, addr) = start(false, 1).await;
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    assert_eq!(line(&mut r).await, "ERR INVALID-ARGUMENT\n");
    assert_eq!(line(&mut r).await, "");
    let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
    r.get_mut().write_all(b"VER\n").await.unwrap();
    assert!(line(&mut r).await.contains("Network UPS Tools"));
    state.remove_server(id).await;
    assert_eq!(line(&mut r).await, "");
}
#[tokio::test]
async fn connection_cap_refuses_the_257th_peer() {
    let (state, id, addr) = start(false, 300).await;
    let mut peers = Vec::new();
    for _ in 0..netget::server::accept_bounded::DEFAULT_MAX_CONNECTIONS {
        let mut r = BufReader::new(TcpStream::connect(addr).await.unwrap());
        r.get_mut().write_all(b"VER\n").await.unwrap();
        assert!(line(&mut r).await.contains("Network UPS Tools"));
        peers.push(r);
    }
    let mut refused = BufReader::new(TcpStream::connect(addr).await.unwrap());
    assert_eq!(line(&mut refused).await, "ERR ACCESS-DENIED\n");
    state.remove_server(id).await;
}

#[tokio::test]
async fn mocked_model_supplies_values_and_protocol_errors() -> crate::server::helpers::E2EResult<()>
{
    use crate::server::helpers::{start_netget_server, NetGetConfig};
    let config=NetGetConfig::new("listen via nut on port {AVAILABLE_PORT}").with_mock(|mock| {
        mock.on_instruction_containing("via nut").respond_with_actions(json!([{"type":"open_server","port":0,"base_stack":"nut","instruction":"UPS model"}])).expect_calls(1).and()
            .on_event("nut_request").respond_with_actions_from_event(|e| {
                if e["ups"]=="rack1" { json!([{"type":"nut_reply","value":"OB LB"}]) }
                else { json!([{"type":"nut_reply","error":"UNKNOWN-UPS"}]) }
            }).expect_calls(2).and()
    });
    let server = start_netget_server(config).await?;
    let mut r = BufReader::new(TcpStream::connect(("127.0.0.1", server.port)).await?);
    r.get_mut()
        .write_all(b"GET VAR rack1 ups.status\nGET VAR other ups.status\nLOGOUT\n")
        .await?;
    assert_eq!(line(&mut r).await, "VAR rack1 ups.status \"OB LB\"\n");
    assert_eq!(line(&mut r).await, "ERR UNKNOWN-UPS\n");
    assert_eq!(line(&mut r).await, "OK Goodbye\n");
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    Ok(())
}
